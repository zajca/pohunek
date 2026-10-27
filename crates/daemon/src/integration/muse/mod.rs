//! The Muse Code hook handler.
//!
//! Muse runs hooks with a cleared environment and reads managed hooks from a
//! file named by `managed_hooks_path`. Pohunek installs nothing persistent
//! for it: every launch gets a session-private overlay file, selected through
//! `TBH_MANAGED_HOOKS_PATH`, and the launch arguments `-c
//! managed_hooks_env_vars=[...]` that let the stateless reporter see the
//! `POHUNEK_*` names it reads. Both come from [`MuseHook`] as launch input
//! (see [`super::launch_input`]); `integration install`, `status` and
//! `uninstall` only inspect and never write to the Muse config home.
//!
//! The handler is self-contained: removing Muse support deletes this module
//! and its rows in the handler registry, the hook schema provider lists and
//! the `mod muse;` line and test accessor in the integration module, the
//! `muse_hook` scenarios with their `mod` line in the session tests, the
//! `thread_environment_variable` test helper and the handler count in the
//! closed-set assertion of `handler_tests`. The
//! `integration-muse-hook-v1` row of the append-only feature table stays: a
//! descriptor that names `muse-hook-v1` is then refused because the build
//! provides no such handler.
//!
//! `status` and `install` check the config home only (the settings file and the
//! overlay it would produce), so a managed hooks file named by
//! `TBH_MANAGED_HOOKS_PATH` in a profile `[env]` or the service environment is
//! read at launch, not by them.
//!
//! UNVERIFIED: the managed hooks file layout, the accepted `-c` spellings, the
//! hook payload fields and the hook process ancestry are taken from the Muse
//! documentation and the MIT Muse Code SDK (release 1.4.13), not from a capture
//! of a real Muse run.

// Rust guideline compliant 2026-10-27

mod overlay;

use std::path::Path;

use pohunek_worker_protocol::HookAction;
use protocol::{
    ErrorClass, IntegrationAgentStatus, IntegrationInstallState, IntegrationRecovery,
    IntegrationUninstallReport, IntegrationUninstallState, ProtocolError, RuntimeRef,
    EXPECTED_INTEGRATION_VERSION,
};

use super::handler::{AssetManifest, DaemonHandler, StagedUpdate};
use super::launch_input::{
    FileKind, LaunchContributor, LaunchInput, LaunchInputContext, PrivateFile,
};
use super::{InstallPaths, UNRESOLVED_HOME_WARNING};
use crate::integration::commit::StepGate;
use overlay::{Inputs, Problem};

/// Registry id of the handler.
pub(super) const HANDLER_ID: &str = "muse-hook-v1";

/// Runtime id the handler reports for.
const RUNTIME_ID: &str = "muse";

/// Version marker of the reporter asset.
const ASSET_VERSION: u32 = 1;

/// Hook operations the reporter sends.
const REPORTED_ACTIONS: &[HookAction] = &[
    HookAction::IdentityReport,
    HookAction::SubagentStart,
    HookAction::SubagentStop,
];

/// Name of the per-session reporter script below the launch-input prefix.
const REPORTER_FILE: &str = "muse-reporter.sh";

/// Name of the per-session managed hooks overlay below the launch-input prefix.
const OVERLAY_FILE: &str = "muse-hooks.json";

/// The reporter script every launch writes.
const REPORTER: &str = include_str!("reporter.sh");

/// Error code of a launch refused because the overlay cannot be built.
pub(super) const OVERLAY_UNAVAILABLE_CODE: &str = "integration_overlay_unavailable";

/// Error code of a launch refused because its arguments already set the key the
/// overlay needs.
pub(super) const ARG_CONFLICT_CODE: &str = "integration_launch_arg_conflict";

/// The Muse hook handler and its launch contributor.
#[derive(Debug)]
pub(super) struct MuseHook;

/// The registered instance.
pub(super) static MUSE_HOOK: MuseHook = MuseHook;

impl LaunchContributor for MuseHook {
    fn contribute(&self, ctx: &LaunchInputContext<'_>) -> Result<LaunchInput, ProtocolError> {
        overlay::refuse_conflicting_args(ctx.args)?;
        // Without a declared config home the settings file, which may name a
        // policy hooks file the overlay must keep, cannot be found.
        if ctx.config_home.is_none() {
            return Err(overlay_unavailable(&Problem::NoConfigHome));
        }
        let inputs = Inputs::read(ctx.config_home, ctx.env)
            .map_err(|problem| overlay_unavailable(&problem))?;
        let reporter = ctx.file_path(REPORTER_FILE);
        let hooks =
            overlay::build(&inputs, &reporter).map_err(|problem| overlay_unavailable(&problem))?;
        Ok(LaunchInput {
            env: vec![(
                overlay::MANAGED_PATH_ENV.to_owned(),
                format!("{{file:{OVERLAY_FILE}}}"),
            )],
            root_args: vec!["-c".to_owned(), overlay::env_vars_arg(&inputs)],
            files: vec![
                PrivateFile {
                    name: OVERLAY_FILE.to_owned(),
                    bytes: hooks,
                    kind: FileKind::Data,
                },
                PrivateFile {
                    name: REPORTER_FILE.to_owned(),
                    bytes: REPORTER.as_bytes().to_vec(),
                    kind: FileKind::Executable,
                },
            ],
        })
    }
}

/// The `POHUNEK_*` names the reporter reads from the hook environment.
#[cfg(test)]
pub(super) fn hook_env_vars() -> &'static [&'static str] {
    overlay::HOOK_ENV_VARS
}

/// Reads the config home `dir` as a launch would, without a profile or service
/// environment, and builds the overlay it would produce.
fn check_home(dir: &Path) -> Result<(), Problem> {
    overlay::validate(&Inputs::read(Some(dir), &[])?)
}

/// `integration_overlay_unavailable` for `problem`.
fn overlay_unavailable(problem: &Problem) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        OVERLAY_UNAVAILABLE_CODE,
        format!("the Muse hook overlay cannot be built: {problem}"),
        Some(
            "repair or remove the file the message names; Pohunek never drops a managed hooks file, because it may carry policy hooks"
                .to_owned(),
        ),
    )
}

/// A staged update that changes nothing: the overlay is per launch.
#[derive(Debug)]
struct StagedMuse {
    manifest: AssetManifest,
}

impl StagedUpdate for StagedMuse {
    fn manifest(&self) -> &AssetManifest {
        &self.manifest
    }

    fn activate(self: Box<Self>, _gate: StepGate<'_>) -> Result<InstallPaths, ProtocolError> {
        Ok(InstallPaths {
            hook_path: std::path::PathBuf::new(),
            config_paths: Vec::new(),
            cleanup_incomplete: Vec::new(),
        })
    }
}

impl DaemonHandler for MuseHook {
    fn id(&self) -> &'static str {
        HANDLER_ID
    }

    fn reported_actions(&self) -> &'static [HookAction] {
        REPORTED_ACTIONS
    }

    fn quarantine_subdirs(&self) -> &'static [&'static str] {
        &[]
    }

    fn inspect_provider(&self, home: Result<&Path, &ProtocolError>) -> IntegrationAgentStatus {
        let agent = RuntimeRef::from_wire(RUNTIME_ID);
        let status = |available, state, recovery, warnings: Vec<String>| IntegrationAgentStatus {
            agent: agent.clone(),
            available,
            expected_asset_paths: Vec::new(),
            present_asset_paths: Vec::new(),
            registration_paths: Vec::new(),
            installed_version: None,
            expected_version: EXPECTED_INTEGRATION_VERSION,
            state,
            recovery,
            warnings,
            home: None,
        };
        let dir = match home {
            Ok(dir) => dir,
            Err(error) => {
                let detail = match &error.recover {
                    Some(recover) => format!("({}; {recover})", error.code),
                    None => format!("({})", error.code),
                };
                return status(
                    false,
                    IntegrationInstallState::Outdated,
                    IntegrationRecovery::RepairConfiguration,
                    vec![format!("{UNRESOLVED_HOME_WARNING} {detail}")],
                );
            }
        };
        if !dir.is_dir() {
            return status(
                false,
                IntegrationInstallState::NotInstalled,
                IntegrationRecovery::None,
                Vec::new(),
            );
        }
        match check_home(dir) {
            Ok(()) => {
                let mut current = status(
                    true,
                    IntegrationInstallState::Current,
                    IntegrationRecovery::None,
                    Vec::new(),
                );
                current.expected_version = ASSET_VERSION;
                current.installed_version = Some(ASSET_VERSION);
                current
            }
            Err(problem) => status(
                true,
                IntegrationInstallState::Outdated,
                IntegrationRecovery::RepairConfiguration,
                vec![problem.to_string()],
            ),
        }
    }

    fn stage(&self, dir: &Path) -> Result<Box<dyn StagedUpdate>, ProtocolError> {
        if !dir.is_dir() {
            return Err(ProtocolError::new(
                ErrorClass::Runtime,
                "agent_config_dir_missing",
                format!(
                    "Muse Code config directory {} does not exist",
                    dir.display()
                ),
                Some("install Muse Code first".to_owned()),
            ));
        }
        check_home(dir).map_err(|problem| overlay_unavailable(&problem))?;
        Ok(Box::new(StagedMuse {
            manifest: AssetManifest {
                version: ASSET_VERSION,
                active_version: Some(ASSET_VERSION),
                actions: REPORTED_ACTIONS.to_vec(),
            },
        }))
    }

    fn uninstall(
        &self,
        runtime: &RuntimeRef,
        _dir: &Path,
    ) -> Result<IntegrationUninstallReport, ProtocolError> {
        Ok(IntegrationUninstallReport {
            agent: runtime.clone(),
            state: IntegrationUninstallState::NotInstalled,
            removed_paths: Vec::new(),
            updated_paths: Vec::new(),
            preserved_paths: Vec::new(),
            cleanup_incomplete: Vec::new(),
            home: None,
        })
    }

    fn launch_contributor(&self) -> Option<&'static dyn LaunchContributor> {
        Some(&MUSE_HOOK)
    }
}
