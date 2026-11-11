//! The Cline hook handler.
//!
//! Cline reads hook files named after the event from `<CLINE_DIR>/hooks/` and
//! runs every file of an event, for every turn on the host. The handler
//! installs one reporter under six event names and keeps the session in the
//! TUI's process tree through per-launch input (see [`launch`]), so a hook of a
//! Pohunek session descends from the PTY.
//!
//! A path in `hooks/` that holds a file without the ownership marker belongs to
//! the user: an install that meets one refuses before writing anything, and
//! uninstall leaves it byte-identical. A file of the same event under another
//! extension is not a collision, because Cline runs both.
//!
//! The handler is self-contained: removing Cline support deletes this module
//! and its rows in the handler registry, the hook schema provider lists and the
//! `mod cline;` line in the integration module, and the `cline_hook`
//! scenarios. The `integration-cline-hook-v1` row of the append-only feature
//! table stays.

// Rust guideline compliant 2026-11-06

mod launch;

use std::path::{Path, PathBuf};

use pohunek_worker_protocol::HookAction;
use protocol::{
    ErrorClass, IntegrationAgentStatus, IntegrationInstallState, IntegrationRecovery,
    IntegrationUninstallReport, IntegrationUninstallState, ProtocolError, RuntimeRef,
};

use super::commit::{self, Action, Expected, Operation, Outcome, Step, StepGate};
use super::handler::{AssetManifest, DaemonHandler, StagedUpdate};
use super::launch_input::LaunchContributor;
use super::{
    active_version, config_dir_is_symlink, config_path_kind, parse_integration_version,
    validate_config_dir, ConfigPath, InstallPaths, InstallerLock, LoadedFile, TrustedDir,
    MANAGED_HOOK_DIR_MODE, MANAGED_HOOK_MODE, UNRESOLVED_HOME_WARNING,
};
use launch::ClineLaunch;

/// Registry id of the handler.
pub(super) const HANDLER_ID: &str = "cline-hook-v1";

/// Runtime id the handler reports for.
const RUNTIME_ID: &str = "cline";

/// Display name used in messages.
const DISPLAY_NAME: &str = "Cline CLI";

/// Version marker of the reporter asset.
const ASSET_VERSION: u32 = 1;

/// The reporter every event file carries.
const REPORTER: &str = include_str!("reporter.sh");

/// Line of its own that marks a file as installed by this handler.
const OWNERSHIP_MARKER: &str = "# POHUNEK_INTEGRATION_ID=cline";

/// Directory of the config home that Cline reads hook files from.
const HOOKS_DIR: &str = "hooks";

/// Hook file names, one per event Cline runs a reporter for. Cline matches the
/// base name case-insensitively and accepts several extensions; `.sh` is the
/// one this handler owns.
const HOOK_FILES: [&str; 6] = [
    "TaskStart.sh",
    "TaskResume.sh",
    "TaskComplete.sh",
    "TaskError.sh",
    "TaskCancel.sh",
    "SessionShutdown.sh",
];

/// Hook operations the reporter sends.
const REPORTED_ACTIONS: &[HookAction] = &[
    HookAction::IdentityReport,
    HookAction::SubagentStart,
    HookAction::SubagentStop,
];

/// Error code of an install that meets a file it does not own.
pub(super) const PATH_CONFLICT_CODE: &str = "integration_hook_path_conflict";

/// The Cline hook handler.
#[derive(Debug)]
pub(super) struct ClineHook;

/// The registered instance.
pub(super) static CLINE_HOOK: ClineHook = ClineHook;

static LAUNCH: ClineLaunch = ClineLaunch;

/// Whether `content` carries the ownership marker on a line of its own.
fn owns(content: &str) -> bool {
    content.lines().any(|line| line.trim() == OWNERSHIP_MARKER)
}

/// `agent_config_dir_missing` for `dir`.
fn config_dir_missing(dir: &Path) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "agent_config_dir_missing",
        format!(
            "{DISPLAY_NAME} config directory {} does not exist",
            dir.display()
        ),
        Some(format!("install {DISPLAY_NAME} first")),
    )
}

/// `integration_hook_path_conflict` naming the paths a foreign file holds.
fn path_conflict(paths: &[PathBuf]) -> ProtocolError {
    let names: Vec<String> = paths
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    ProtocolError::new(
        ErrorClass::Runtime,
        PATH_CONFLICT_CODE,
        format!(
            "{DISPLAY_NAME} hook files that Pohunek does not own are in the way: {}",
            names.join(", ")
        ),
        Some(
            "move or rename those files (Cline runs hook files under other extensions too), then run the install again; Pohunek never overwrites them"
                .to_owned(),
        ),
    )
}

/// A Cline asset set computed against the current config directory and held
/// under the installer lock.
#[derive(Debug)]
struct StagedCline {
    config_root: TrustedDir,
    hooks_dir: PathBuf,
    trusted_hooks: Option<TrustedDir>,
    loaded: Vec<Option<LoadedFile>>,
    manifest: AssetManifest,
    _install_lock: InstallerLock,
}

impl StagedUpdate for StagedCline {
    fn manifest(&self) -> &AssetManifest {
        &self.manifest
    }

    fn activate(self: Box<Self>, gate: StepGate<'_>) -> Result<InstallPaths, ProtocolError> {
        let Self {
            config_root,
            hooks_dir,
            trusted_hooks,
            loaded,
            _install_lock,
            ..
        } = *self;
        let created_hooks = trusted_hooks.is_none();
        let trusted_hooks = match trusted_hooks {
            Some(hooks) => hooks,
            None => config_root.create_child(
                HOOKS_DIR,
                "Cline hooks directory",
                MANAGED_HOOK_DIR_MODE,
            )?,
        };
        let steps: Vec<Step<'_>> = HOOK_FILES
            .iter()
            .zip(&loaded)
            .map(|(name, before)| Step {
                dir: &trusted_hooks,
                name,
                label: "Cline hook",
                action: Action::Write {
                    body: REPORTER,
                    mode: MANAGED_HOOK_MODE,
                },
                expected: Expected::Loaded(before.as_ref()),
            })
            .collect();
        let committed = commit::commit(&steps, gate, Operation::Install);
        if let Err(error) = &committed {
            // A directory this install created is removed again once the rollback
            // left it empty; after an incomplete rollback it stays for inspection.
            if created_hooks && error.code != commit::RECOVERY_REQUIRED_CODE {
                if let Err(quarantine) =
                    config_root.remove_created_empty_child(HOOKS_DIR, &trusted_hooks)
                {
                    return Err(commit::recovery_required(
                        &[format!(
                            "{} (the created hooks directory and any files in it stay at {})",
                            hooks_dir.display(),
                            quarantine.display()
                        )],
                        Some(&error.code),
                        Operation::Install,
                    ));
                }
            }
        }
        let committed = committed?;
        Ok(InstallPaths {
            hook_path: hooks_dir.clone(),
            config_paths: HOOK_FILES.iter().map(|name| hooks_dir.join(name)).collect(),
            cleanup_incomplete: committed.cleanup_incomplete,
        })
    }
}

/// Whether `path` is absent or a regular file, as opposed to a symlink or
/// another kind of entry.
fn is_plain_entry(path: &Path) -> bool {
    std::fs::symlink_metadata(path).map_or(true, |metadata| metadata.is_file())
}

/// Computes the asset set for the config directory `dir` without changing it.
fn stage_cline(dir: &Path) -> Result<StagedCline, ProtocolError> {
    let label = "Cline config directory";
    validate_config_dir(dir.to_path_buf(), label)?;
    if config_path_kind(dir) == ConfigPath::Symlink {
        return Err(config_dir_is_symlink(dir));
    }
    if !dir.is_dir() {
        return Err(config_dir_missing(dir));
    }
    let config_root = TrustedDir::open(dir, label)?;
    let install_lock = config_root.lock_installer()?;
    let hooks_dir = dir.join(HOOKS_DIR);
    let trusted_hooks = config_root.open_child(HOOKS_DIR, "Cline hooks directory")?;
    let mut loaded = Vec::with_capacity(HOOK_FILES.len());
    let mut foreign = Vec::new();
    for name in HOOK_FILES {
        let file = match &trusted_hooks {
            Some(hooks) => match hooks.read_optional(name, "Cline hook") {
                Ok(file) => file,
                // A symlink or a directory is as much in the way as a foreign
                // file; any other read failure is reported as it is.
                Err(_untrusted) if !is_plain_entry(&hooks_dir.join(name)) => {
                    foreign.push(hooks_dir.join(name));
                    loaded.push(None);
                    continue;
                }
                Err(error) => return Err(error),
            },
            None => None,
        };
        match file {
            Some(file) if !owns(&file.content) => {
                foreign.push(hooks_dir.join(name));
                loaded.push(None);
            }
            other => loaded.push(other),
        }
    }
    if !foreign.is_empty() {
        return Err(path_conflict(&foreign));
    }
    let manifest = AssetManifest {
        version: ASSET_VERSION,
        active_version: active_version(trusted_hooks.as_ref(), HOOK_FILES[0], "Cline hook"),
        actions: REPORTED_ACTIONS.to_vec(),
    };
    Ok(StagedCline {
        config_root,
        hooks_dir,
        trusted_hooks,
        loaded,
        manifest,
        _install_lock: install_lock,
    })
}

/// Installs the reporter into the config directory `dir`.
///
/// # Errors
///
/// The errors of staging and activating the asset set.
#[cfg(test)]
pub(super) fn install(dir: &Path) -> Result<InstallPaths, ProtocolError> {
    Box::new(stage_cline(dir)?).activate(&mut |_index, _name| Ok(()))
}

/// What one hook path holds.
enum PathState {
    Absent,
    Current,
    /// A file this handler wrote whose bytes or mode differ from the reporter,
    /// with the version marker it carries.
    Drifted(Option<u32>),
    Foreign,
    Unreadable(String),
}

/// Reads the state of the hook file `name` in `hooks`.
fn path_state(hooks: &TrustedDir, name: &str) -> PathState {
    match hooks.read_optional(name, "Cline hook") {
        Ok(None) => PathState::Absent,
        Ok(Some(file)) if !owns(&file.content) => PathState::Foreign,
        Ok(Some(file)) => {
            let executable = file.mode & 0o100 != 0 && file.mode & 0o022 == 0;
            if file.content == REPORTER && executable {
                PathState::Current
            } else {
                PathState::Drifted(parse_integration_version(&file.content))
            }
        }
        Err(error) => PathState::Unreadable(format!("{} ({})", error.msg, error.code)),
    }
}

/// Command that repairs a drifted or partial install.
const REINSTALL_HINT: &str = "run `pohunek integration install --agent cline`";

/// A status of the Cline runtime with no asset findings.
fn status_with(
    available: bool,
    state: IntegrationInstallState,
    recovery: IntegrationRecovery,
    warnings: Vec<String>,
) -> IntegrationAgentStatus {
    IntegrationAgentStatus {
        skill: None,
        agent: RuntimeRef::from_wire(RUNTIME_ID),
        available,
        expected_asset_paths: Vec::new(),
        present_asset_paths: Vec::new(),
        registration_paths: Vec::new(),
        installed_version: None,
        expected_version: ASSET_VERSION,
        state,
        recovery,
        warnings,
        home: None,
    }
}

/// The status of the six hook paths below `hooks_dir`, `hooks` being that
/// directory when it exists.
fn inspect_hooks(hooks_dir: &Path, hooks: Option<&TrustedDir>) -> IntegrationAgentStatus {
    let mut warnings = Vec::new();
    let mut recovery = IntegrationRecovery::None;
    let mut present = Vec::new();
    let mut versions = Vec::new();
    let mut current = 0usize;
    let mut absent = 0usize;
    for name in HOOK_FILES {
        let path = hooks_dir.join(name).display().to_string();
        match hooks.map_or(PathState::Absent, |hooks| path_state(hooks, name)) {
            PathState::Absent => absent += 1,
            PathState::Current => {
                current += 1;
                present.push(path);
                versions.push(Some(ASSET_VERSION));
            }
            PathState::Drifted(version) => {
                warnings.push(format!(
                    "{path} differs from the reporter this daemon embeds; {REINSTALL_HINT}"
                ));
                present.push(path);
                versions.push(version);
            }
            PathState::Foreign => {
                warnings.push(format!(
                    "{path} is not a Pohunek hook file, so the reporter cannot be installed there; move it, then {REINSTALL_HINT}"
                ));
                recovery = IntegrationRecovery::RepairConfiguration;
            }
            PathState::Unreadable(why) => {
                warnings.push(format!("{path} cannot be inspected: {why}"));
                recovery = IntegrationRecovery::RepairConfiguration;
            }
        }
    }
    let installed_version = match versions.split_first() {
        Some((first, rest)) if rest.iter().all(|other| other == first) => *first,
        _ => None,
    };
    let state = if current == HOOK_FILES.len() {
        IntegrationInstallState::Current
    } else if absent == HOOK_FILES.len() && warnings.is_empty() {
        IntegrationInstallState::NotInstalled
    } else {
        IntegrationInstallState::Outdated
    };
    if state == IntegrationInstallState::Outdated && recovery == IntegrationRecovery::None {
        recovery = IntegrationRecovery::Reinstall;
        if warnings.is_empty() {
            warnings.push(format!(
                "{absent} of {} Cline hook files are missing; {REINSTALL_HINT}",
                HOOK_FILES.len()
            ));
        }
    }
    let mut status = status_with(true, state, recovery, warnings);
    status.expected_asset_paths = HOOK_FILES
        .iter()
        .map(|name| hooks_dir.join(name).display().to_string())
        .collect();
    status.present_asset_paths = present;
    status.installed_version = installed_version;
    status
}

impl DaemonHandler for ClineHook {
    fn id(&self) -> &'static str {
        HANDLER_ID
    }

    fn reported_actions(&self) -> &'static [HookAction] {
        REPORTED_ACTIONS
    }

    fn quarantine_subdirs(&self) -> &'static [&'static str] {
        &[HOOKS_DIR]
    }

    fn inspect_provider(&self, home: Result<&Path, &ProtocolError>) -> IntegrationAgentStatus {
        let dir = match home {
            Ok(dir) => dir,
            Err(error) => {
                let detail = match &error.recover {
                    Some(recover) => format!("({}; {recover})", error.code),
                    None => format!("({})", error.code),
                };
                return status_with(
                    false,
                    IntegrationInstallState::Outdated,
                    IntegrationRecovery::RepairConfiguration,
                    vec![format!("{UNRESOLVED_HOME_WARNING} {detail}")],
                );
            }
        };
        if !dir.is_dir() {
            return status_with(
                false,
                IntegrationInstallState::NotInstalled,
                IntegrationRecovery::None,
                Vec::new(),
            );
        }
        let hooks = TrustedDir::open(dir, "Cline config directory")
            .and_then(|root| root.open_child(HOOKS_DIR, "Cline hooks directory"));
        match hooks {
            Ok(hooks) => inspect_hooks(&dir.join(HOOKS_DIR), hooks.as_ref()),
            Err(error) => status_with(
                true,
                IntegrationInstallState::Outdated,
                IntegrationRecovery::RepairConfiguration,
                vec![format!("{} ({})", error.msg, error.code)],
            ),
        }
    }

    fn stage(&self, dir: &Path) -> Result<Box<dyn StagedUpdate>, ProtocolError> {
        Ok(Box::new(stage_cline(dir)?))
    }

    fn uninstall(
        &self,
        runtime: &RuntimeRef,
        dir: &Path,
    ) -> Result<IntegrationUninstallReport, ProtocolError> {
        uninstall_cline(runtime, dir, &mut |_index, _name| Ok(()))
    }

    fn launch_contributor(&self) -> Option<&'static dyn LaunchContributor> {
        Some(&LAUNCH)
    }
}

/// Removes the hook files of `dir` that carry the ownership marker and leaves
/// every other entry untouched.
fn uninstall_cline(
    runtime: &RuntimeRef,
    dir: &Path,
    gate: StepGate<'_>,
) -> Result<IntegrationUninstallReport, ProtocolError> {
    let mut report = IntegrationUninstallReport {
        skill: None,
        agent: runtime.clone(),
        state: IntegrationUninstallState::NotInstalled,
        removed_paths: Vec::new(),
        updated_paths: Vec::new(),
        preserved_paths: Vec::new(),
        cleanup_incomplete: Vec::new(),
        home: None,
    };
    let label = "Cline config directory";
    validate_config_dir(dir.to_path_buf(), label)?;
    match config_path_kind(dir) {
        ConfigPath::Directory => {}
        ConfigPath::Absent => return Ok(report),
        ConfigPath::Symlink => return Err(config_dir_is_symlink(dir)),
        ConfigPath::Other | ConfigPath::Unreadable => {
            return Err(super::path_untrusted(
                dir,
                "agent config path is not a readable directory",
            ));
        }
    }
    let root = TrustedDir::open(dir, label)?;
    let _lock = root.lock_installer()?;
    let Some(hooks) = root.open_child(HOOKS_DIR, "Cline hooks directory")? else {
        return Ok(report);
    };
    let steps: Vec<Step<'_>> = HOOK_FILES
        .iter()
        .map(|name| Step {
            dir: &hooks,
            name,
            label: "Cline hook",
            action: Action::RemoveOwned {
                marker: OWNERSHIP_MARKER,
            },
            expected: Expected::Any,
        })
        .collect();
    let committed = commit::commit(&steps, gate, Operation::Uninstall)?;
    report.cleanup_incomplete = committed.cleanup_incomplete;
    for (name, outcome) in HOOK_FILES.iter().zip(&committed.outcomes) {
        let path = dir.join(HOOKS_DIR).join(name).display().to_string();
        match outcome {
            Outcome::Removed => report.removed_paths.push(path),
            Outcome::Foreign => report.preserved_paths.push(path),
            Outcome::Written | Outcome::Absent | Outcome::Verified => {}
        }
    }
    if !report.removed_paths.is_empty() {
        report.state = IntegrationUninstallState::Removed;
    }
    Ok(report)
}
