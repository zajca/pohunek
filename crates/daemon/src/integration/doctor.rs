//! Read-only diagnosis of the Claude and Codex hook integrations.
//!
//! The doctor turns the status inspection into stable, remediable findings and
//! adds two checks status does not make: an informational note about `python3`
//! on the daemon's own `PATH`, and whether the daemon's runtime socket path is
//! one a hook can connect to. The daemon cannot know the agent's `PATH` or
//! working directory, so the `python3` note is never an error and never affects
//! `ok`. Nothing is written and no interpreter is ever spawned: `python3` is
//! located by path checks alone, because on macOS the stub at `/usr/bin/python3`
//! opens an installer dialog when run without the Command Line Tools.

use std::fs;
use std::path::PathBuf;

use pohunek_paths::{BasePaths, PathEnv, Platform};
use pohunek_platform::filesystem::{FileLock, FsError, LockKind, TrustedDir};
use protocol::{
    IntegrationAgentDoctor, IntegrationAgentStatus, IntegrationDoctorParams,
    IntegrationDoctorResult, IntegrationFinding, IntegrationFindingCode,
    IntegrationFindingSeverity, IntegrationInstallState, IntegrationRecovery, ProtocolError,
    RuntimeRef,
};

use super::handler;
use super::homes::{ConfigHomes, HomeSelection, Target};
use super::{GROUP_OR_OTHER_WRITE_MASK, INSTALL_LOCK_MODE, INSTALL_LOCK_NAME};
#[cfg(test)]
use crate::agent::host::RuntimeHost;

/// File name every hook looks up for its interpreter.
const PYTHON_EXECUTABLE_NAME: &str = "python3";

/// Location of the macOS `python3` stub that needs the Command Line Tools.
const MACOS_PYTHON_STUB: &str = "/usr/bin/python3";

/// Real interpreters the macOS stub delegates to once developer tools exist.
const MACOS_DEVELOPER_PYTHONS: [&str; 2] = [
    "/Library/Developer/CommandLineTools/usr/bin/python3",
    "/Applications/Xcode.app/Contents/Developer/usr/bin/python3",
];

/// Name prefixes of the quarantine entries an install or removal can leave in
/// an agent config directory when a cleanup or rollback did not finish.
const QUARANTINE_PREFIXES: [&str; 3] = [
    ".pohunek-integration-displaced-",
    ".pohunek-integration-rollback-",
    ".pohunek-integration-created-",
];

/// Prefixes the platform primitives give to staged, parked, and stale entries.
const PLATFORM_QUARANTINE_PREFIXES: &[&str] =
    pohunek_platform::filesystem::QUARANTINE_NAME_PREFIXES;

/// Most directory entries scanned per directory for quarantine names.
const MAX_SCANNED_QUARANTINE_ENTRIES: usize = 4096;

/// Most quarantine paths a finding lists before summarizing the rest.
const MAX_LISTED_QUARANTINE_PATHS: usize = 8;

/// Longest valid worker session id, used to check the worker socket path fits.
const LONGEST_WORKER_SESSION_ID: &str = "s-00000000000000000000000000";

/// How `python3` was resolved by path checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PythonState {
    /// The first executable `python3` is a real interpreter, at this path.
    Found(PathBuf),
    /// No executable `python3` exists on the searched `PATH`.
    Missing,
    /// The first `python3` is the macOS stub and no developer tools back it.
    MacosStubOnly,
}

/// The macOS `python3` stub and the interpreters that make it runnable.
#[derive(Debug, Clone)]
pub(super) struct MacosStub {
    pub(super) stub: PathBuf,
    pub(super) developer_pythons: Vec<PathBuf>,
}

/// Locates `python3` the way the hook's shell does, without executing anything.
#[derive(Debug, Clone)]
pub(super) struct PythonProbe {
    /// `PATH` entries in order; the first executable `python3` behind an
    /// absolute entry decides, and relative or empty entries are skipped.
    pub(super) search_dirs: Vec<PathBuf>,
    /// The macOS stub description, on macOS only.
    pub(super) macos_stub: Option<MacosStub>,
}

/// Whether `path` is an executable the daemon would run: the shared
/// trusted-executable check, so this probe agrees with the capability snapshot
/// and the spawn path. The file is never run.
///
/// A file that fails is skipped exactly as a shell skips it while searching
/// `PATH`, so it neither counts as the interpreter nor stops the search.
fn is_executable_file(path: &std::path::Path) -> bool {
    hostcheck::is_executable_file(path)
}

impl PythonProbe {
    /// Builds the probe for this process from its `PATH` entries and, on macOS,
    /// the stub description.
    pub(super) fn from_environment() -> Self {
        let search_dirs = std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect())
            .unwrap_or_default();
        Self {
            search_dirs,
            macos_stub: cfg!(target_os = "macos").then(|| MacosStub {
                stub: PathBuf::from(MACOS_PYTHON_STUB),
                developer_pythons: MACOS_DEVELOPER_PYTHONS.map(PathBuf::from).to_vec(),
            }),
        }
    }

    /// Whether `candidate` is the stub itself or any alias of it (a symlink or
    /// hard link, or an unnormalized path): the file it resolves to is compared
    /// by device and inode, so how the `PATH` entry is spelled never hides it.
    fn is_stub(&self, candidate: &std::path::Path) -> bool {
        use std::os::unix::fs::MetadataExt as _;

        let Some(stub) = &self.macos_stub else {
            return false;
        };
        if stub.stub == candidate {
            return true;
        }
        match (fs::metadata(candidate), fs::metadata(&stub.stub)) {
            (Ok(found), Ok(known)) => found.dev() == known.dev() && found.ino() == known.ino(),
            _ => false,
        }
    }

    /// Resolves `python3` by first match on the searched `PATH`.
    pub(super) fn state(&self) -> PythonState {
        let first = self
            .search_dirs
            .iter()
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(PYTHON_EXECUTABLE_NAME))
            .find(|candidate| is_executable_file(candidate));
        match first {
            None => PythonState::Missing,
            Some(candidate) if self.is_stub(&candidate) => {
                let backed = self.macos_stub.as_ref().is_some_and(|stub| {
                    stub.developer_pythons
                        .iter()
                        .any(|path| is_executable_file(path))
                });
                if backed {
                    PythonState::Found(candidate)
                } else {
                    PythonState::MacosStubOnly
                }
            }
            Some(candidate) => PythonState::Found(candidate),
        }
    }
}

fn error_finding(
    code: IntegrationFindingCode,
    summary: impl Into<String>,
    remediation: impl Into<String>,
) -> IntegrationFinding {
    IntegrationFinding {
        code,
        severity: IntegrationFindingSeverity::Error,
        summary: summary.into(),
        remediation: Some(remediation.into()),
    }
}

fn info_finding(
    code: IntegrationFindingCode,
    summary: impl Into<String>,
    remediation: Option<String>,
) -> IntegrationFinding {
    IntegrationFinding {
        code,
        severity: IntegrationFindingSeverity::Info,
        summary: summary.into(),
        remediation,
    }
}

/// The informational `python3` note.
///
/// The hooks run `python3` from the agent's own `PATH`, which the daemon cannot
/// see, so this only reports what the first match over the daemon's absolute
/// `PATH` entries is. It is never an error and never changes `ok`.
pub(super) fn python_findings(probe: &PythonProbe) -> Vec<IntegrationFinding> {
    let verify = "the hooks run python3 from the agent's PATH; verify it there";
    vec![match probe.state() {
        PythonState::Found(path) => info_finding(
            IntegrationFindingCode::HookRuntimePythonFound,
            format!(
                "the first python3 on the daemon's PATH is {}; the hooks use the agent's own PATH, which the daemon cannot see",
                path.display()
            ),
            None,
        ),
        PythonState::Missing => info_finding(
            IntegrationFindingCode::HookRuntimeMissing,
            "no python3 was found on the daemon's absolute PATH entries (relative and empty entries are skipped)",
            Some(verify.to_owned()),
        ),
        PythonState::MacosStubOnly => info_finding(
            IntegrationFindingCode::HookRuntimeMacosShim,
            "the first python3 on the daemon's PATH is the macOS Command Line Tools stub and no developer tools back it; it was not executed",
            Some(format!(
                "{verify}; on macOS check `xcode-select -p` and install the Command Line Tools if they are missing"
            )),
        ),
    }]
}

/// Findings about the daemon runtime socket path that hooks dial.
pub(super) fn socket_findings(
    platform: Platform,
    effective_uid: u32,
    env: &PathEnv,
) -> Vec<IntegrationFinding> {
    let remediation = "start pohunekd with a shorter XDG_RUNTIME_DIR so the daemon and worker socket paths fit the platform limit, then restart the daemon";
    match BasePaths::resolve_for(platform, effective_uid, env) {
        Err(error) => vec![error_finding(
            IntegrationFindingCode::HookSocketPathInvalid,
            format!("the daemon runtime socket path cannot be used: {error}"),
            remediation,
        )],
        Ok(paths) => match paths.worker_socket(LONGEST_WORKER_SESSION_ID) {
            Ok(_) => Vec::new(),
            Err(error) => vec![error_finding(
                IntegrationFindingCode::HookSocketPathInvalid,
                format!("a worker socket path would exceed the platform limit: {error}"),
                remediation,
            )],
        },
    }
}

fn socket_findings_for_process() -> Vec<IntegrationFinding> {
    match Platform::current() {
        Ok(platform) => socket_findings(
            platform,
            nix::unistd::Uid::effective().as_raw(),
            &PathEnv::capture(),
        ),
        Err(error) => vec![error_finding(
            IntegrationFindingCode::HookSocketPathInvalid,
            format!("the daemon runtime socket path cannot be resolved: {error}"),
            "run the daemon on a supported platform",
        )],
    }
}

/// Maps one status warning to its cause and remediation.
pub(super) fn classify_warning(
    warning: &str,
    agent_label: &str,
    recovery: IntegrationRecovery,
) -> IntegrationFinding {
    let install =
        format!("run `pohunek integration install --agent {agent_label}` on the daemon host");
    let repair = format!(
        "inspect and repair the file or directory the finding names by hand (owner, mode, type, contents), then {install}"
    );
    let unsafe_fix = if recovery == IntegrationRecovery::RepairConfiguration {
        repair.clone()
    } else {
        install.clone()
    };
    let (code, remediation) = if warning.contains("config path is not a directory")
        || warning.contains("config path is a symlink")
        || warning.contains("config directory failed trusted filesystem validation")
        || warning.contains("config directory could not be")
    {
        (
            IntegrationFindingCode::ConfigRootInvalid,
            "make the runtime's config home an existing absolute directory owned by the daemon user: set the variable its descriptor declares (a host profile sets it in its [env]) or create the default directory below HOME, and use the canonical path when the current one goes through a symlink, then re-run the doctor".to_owned(),
        )
    } else if warning.contains("managed asset parent") {
        (IntegrationFindingCode::AssetUnsafe, repair)
    } else if warning.contains("Codex hooks feature is not enabled") {
        (IntegrationFindingCode::CodexHooksFeatureDisabled, install)
    } else if warning.contains("trust") {
        (IntegrationFindingCode::CodexTrustDrift, install)
    } else if warning.starts_with("managed ") {
        if warning.contains("registration") {
            (IntegrationFindingCode::RegistrationDrift, install)
        } else if [
            "version",
            "content differs",
            "inspection limit",
            "could not be read",
        ]
        .iter()
        .any(|needle| warning.contains(needle))
        {
            (IntegrationFindingCode::AssetModified, install)
        } else if warning.contains("is missing") {
            (IntegrationFindingCode::AssetMissing, install)
        } else if [
            "permissions",
            "owned by",
            "not a regular file",
            "opened safely",
            "metadata could not",
        ]
        .iter()
        .any(|needle| warning.contains(needle))
        {
            (IntegrationFindingCode::AssetUnsafe, unsafe_fix)
        } else {
            (IntegrationFindingCode::AssetModified, install)
        }
    } else if warning.contains("is missing") || warning.contains("no hooks object") {
        (IntegrationFindingCode::RegistrationDrift, install)
    } else {
        (IntegrationFindingCode::ProviderConfigInvalid, repair)
    };
    error_finding(code, warning, remediation)
}

/// Whether a status warning only says the integration is not installed yet.
fn expected_when_not_installed(warning: &str) -> bool {
    [
        "is missing",
        "is not enabled",
        "has no hooks object",
        "trust record is missing or modified",
        "registration is missing or modified",
    ]
    .iter()
    .any(|expected| warning.contains(expected))
}

/// Derives one agent's findings from its status and the shared runtime findings.
#[cfg(test)]
pub(super) fn diagnose(
    status: IntegrationAgentStatus,
    runtime: &[IntegrationFinding],
) -> IntegrationAgentDoctor {
    let label = status.agent.as_wire().to_owned();
    diagnose_as(status, &label, runtime)
}

/// [`diagnose`] naming `label` in recovery commands and prose.
///
/// The warnings are classified exactly as the handler worded them, so the label
/// never takes part in classification.
pub(super) fn diagnose_as(
    status: IntegrationAgentStatus,
    label: &str,
    runtime: &[IntegrationFinding],
) -> IntegrationAgentDoctor {
    let mut findings = Vec::new();
    if status.state == IntegrationInstallState::NotInstalled {
        findings.push(if status.available {
            info_finding(
                IntegrationFindingCode::HooksNotInstalled,
                format!("{label} is present but the Pohunek hooks are not installed"),
                Some(format!(
                    "run `pohunek integration install --agent {label}` on the daemon host to enable them"
                )),
            )
        } else {
            info_finding(
                IntegrationFindingCode::AgentNotInstalled,
                format!("{label} is not installed on this host; nothing to check"),
                None,
            )
        });
        // Only the absence expected of a not-installed integration is muted:
        // anything unsafe, malformed, or needing repair still fails the doctor,
        // because a later install would refuse the same path.
        if status.available {
            findings.extend(
                status
                    .warnings
                    .iter()
                    .filter(|warning| !expected_when_not_installed(warning))
                    .map(|warning| classify_warning(warning, label, status.recovery)),
            );
        }
    } else {
        for warning in &status.warnings {
            findings.push(classify_warning(warning, label, status.recovery));
        }
        if status.state == IntegrationInstallState::Outdated && findings.is_empty() {
            findings.push(error_finding(
                IntegrationFindingCode::InstallDrift,
                "the installed integration does not match the installer's contract",
                format!("run `pohunek integration install --agent {label}` on the daemon host"),
            ));
        }
        findings.extend(runtime.iter().cloned());
    }
    let ok = findings
        .iter()
        .all(|finding| finding.severity != IntegrationFindingSeverity::Error);
    IntegrationAgentDoctor {
        agent: status.agent.clone(),
        ok,
        status: Some(status),
        findings,
        home: None,
    }
}

/// Reports `diagnosis` for `runtime`, after its findings were classified.
fn retarget_diagnosis(diagnosis: &mut IntegrationAgentDoctor, runtime: &RuntimeRef) {
    let from = diagnosis.agent.as_wire().to_owned();
    if let Some(status) = &mut diagnosis.status {
        handler::retarget_status(status, runtime);
    }
    if from != runtime.as_wire() {
        for finding in &mut diagnosis.findings {
            finding.summary = handler::retarget_text(&finding.summary, &from, runtime.as_wire());
        }
    }
    diagnosis.agent = runtime.clone();
}

/// Quarantine-prefixed names in `directory`, and whether the scan of it was
/// incomplete (stopped at `limit` entries or could not list the directory).
///
/// Enumeration goes through the trusted descriptor and only lists names, so a
/// symlink is never followed and nothing outside the directory is read. The
/// bound applies to entries scanned before filtering and readdir order is
/// unspecified, so an incomplete scan says nothing about the entries it did
/// not reach.
fn quarantine_names(directory: &TrustedDir, limit: usize) -> (Vec<String>, bool) {
    let Ok((names, truncated)) = directory.entry_names_limited(limit) else {
        return (Vec::new(), true);
    };
    let matching = names
        .into_iter()
        .filter_map(|name| name.into_string().ok())
        .filter(|name| {
            QUARANTINE_PREFIXES
                .iter()
                .chain(PLATFORM_QUARANTINE_PREFIXES)
                .any(|prefix| name.starts_with(prefix))
        })
        .collect();
    (matching, truncated)
}

/// Quarantine entries left in `dir` and in the `subdirs` the handler's asset
/// set can also occupy.
///
/// Only a config root that passes the installer's trusted walk is scanned, and
/// so is only a child that passes the same validation: a symlinked root or
/// child is a config-root or asset finding, never a place to list files.
/// An incomplete scan is its own error finding, so a bounded or failed scan can
/// never read as a clean diagnosis.
pub(super) fn quarantine_findings_in(
    dir: &std::path::Path,
    subdirs: &[&str],
    limit: usize,
) -> Vec<IntegrationFinding> {
    let Ok(root) = TrustedDir::open_absolute_owner_safe(dir, GROUP_OR_OTHER_WRITE_MASK) else {
        return Vec::new();
    };
    let mut left: Vec<PathBuf> = Vec::new();
    let (names, mut incomplete) = quarantine_names(&root, limit);
    left.extend(names.into_iter().map(|name| dir.join(name)));
    for subdir in subdirs {
        if let Ok(child) = root.open_child_owner_safe(subdir, GROUP_OR_OTHER_WRITE_MASK) {
            let (names, cut) = quarantine_names(&child, limit);
            incomplete |= cut;
            left.extend(names.into_iter().map(|name| dir.join(subdir).join(name)));
        }
    }
    let mut findings = Vec::new();
    if !left.is_empty() {
        left.sort();
        let hidden = left.len().saturating_sub(MAX_LISTED_QUARANTINE_PATHS);
        let listed = left
            .iter()
            .take(MAX_LISTED_QUARANTINE_PATHS)
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let more = if hidden == 0 {
            String::new()
        } else {
            format!(" and {hidden} more")
        };
        findings.push(error_finding(
            IntegrationFindingCode::DisplacedOriginalLeftBehind,
            format!(
                "{} quarantined original(s) from an earlier install or removal remain: {listed}{more}",
                left.len()
            ),
            "review each listed entry: it is a file the installer moved aside (a previous version of a managed file or registration); once you have confirmed you do not need it, delete it by hand, or move it back to its original name if the install or removal was interrupted",
        ));
    }
    if incomplete {
        findings.push(error_finding(
            IntegrationFindingCode::QuarantineScanIncomplete,
            format!(
                "the quarantine scan could not cover every entry of the agent config directory (it stops after {limit} entries per directory, or the directory could not be listed), so leftover originals may exist that are not reported"
            ),
            "list the agent config directory (and its managed subdirectories) by hand for entries whose names start with `.pohunek-`, review each, and remove unrelated files so the scan can complete",
        ));
    }
    findings
}

/// The outcome of asking whether an install, uninstall, or doctor is running.
enum LockProbe {
    /// The lock file does not exist, so no operation has ever run here.
    Absent,
    /// Nobody holds the lock; the guard keeps it for the whole inspection.
    Idle(#[expect(dead_code, reason = "held only to keep the lock")] FileLock),
    /// Another install, uninstall, or doctor holds the lock.
    Busy,
    /// The lock file is not a safe regular owner-private file.
    Unsafe,
}

/// Probes the installer lock of `dir` without ever creating it.
///
/// The lock is taken non-blocking and, when free, kept by the returned guard
/// for the whole (short, read-only) inspection, so no operation can run in
/// between; an installer that arrives meanwhile fails fast with a retryable
/// error instead of waiting. A directory that fails the trusted walk cannot
/// host a running operation and counts as absent.
fn probe_installer_lock(dir: &std::path::Path) -> LockProbe {
    let Ok(root) = TrustedDir::open_absolute_owner_safe(dir, GROUP_OR_OTHER_WRITE_MASK) else {
        return LockProbe::Absent;
    };
    match root.lock_existing_file(INSTALL_LOCK_NAME, INSTALL_LOCK_MODE, LockKind::Exclusive) {
        Ok(None) => LockProbe::Absent,
        Ok(Some(guard)) => LockProbe::Idle(guard),
        Err(FsError::LockContended { .. }) => LockProbe::Busy,
        Err(_error) => LockProbe::Unsafe,
    }
}

/// The diagnosis reported when another operation holds the installer lock:
/// nothing was read or scanned, and it never fails the doctor.
fn operation_in_progress(agent: RuntimeRef) -> IntegrationAgentDoctor {
    IntegrationAgentDoctor {
        agent,
        ok: true,
        status: None,
        findings: vec![info_finding(
            IntegrationFindingCode::OperationInProgress,
            "an integration install, uninstall, or doctor is running, so nothing was inspected for this agent",
            Some("run the doctor again once the other operation has finished".to_owned()),
        )],
        home: None,
    }
}

/// The error finding for an installer lock file that is not safe to use.
fn unsafe_installer_lock(dir: &std::path::Path) -> IntegrationFinding {
    error_finding(
        IntegrationFindingCode::UnsafeInstallerLock,
        format!(
            "{} is not a regular owner-private file (a symlink, directory, wrong mode or owner, or a replaced file), so every install and uninstall fails on it",
            dir.join(INSTALL_LOCK_NAME).display()
        ),
        "inspect it; when no install or uninstall is running, delete it (it is recreated with mode 0600) or repair its type and permissions, then run the doctor again",
    )
}

/// Diagnose the selected runtime(s) in the selected config homes through their
/// integration handlers without changing anything.
///
/// # Errors
///
/// `agent_not_installable` for a runtime without a daemon-run handler (the
/// shell, and Hermes, which has its own local doctor), or the typed errors of
/// an unusable profile selection; the API handler rejects historical and
/// uninstalled runtimes before this runs.
pub fn doctor_in(
    homes: &ConfigHomes,
    params: IntegrationDoctorParams,
) -> Result<IntegrationDoctorResult, ProtocolError> {
    doctor_in_with(
        homes,
        params,
        &python_findings(&PythonProbe::from_environment()),
        &socket_findings_for_process(),
    )
}

/// [`doctor_in`] against the built-in runtimes.
#[cfg(test)]
pub fn doctor(params: IntegrationDoctorParams) -> Result<IntegrationDoctorResult, ProtocolError> {
    doctor_for(&crate::agent::host::fixture::builtin_host(), params)
}

/// [`doctor_in`] over `host`, resolving homes against this test process's
/// environment.
#[cfg(test)]
pub(super) fn doctor_for(
    host: &RuntimeHost,
    params: IntegrationDoctorParams,
) -> Result<IntegrationDoctorResult, ProtocolError> {
    doctor_in(&super::config_homes_for_tests(host), params)
}

/// [`doctor_for_with`] against the built-in runtimes.
#[cfg(test)]
pub(super) fn doctor_with(
    params: IntegrationDoctorParams,
    python: &[IntegrationFinding],
    socket: &[IntegrationFinding],
) -> Result<IntegrationDoctorResult, ProtocolError> {
    doctor_for_with(
        &crate::agent::host::fixture::builtin_host(),
        params,
        python,
        socket,
    )
}

/// Quarantine entries left in `dir` by the handler `agent` resolves to in the
/// built-in runtimes.
#[cfg(test)]
pub(super) fn quarantine_findings(
    dir: &std::path::Path,
    agent: &RuntimeRef,
    limit: usize,
) -> Vec<IntegrationFinding> {
    let subdirs = handler::resolve(&crate::agent::host::fixture::builtin_host(), agent)
        .map(|resolved| resolved.handler.quarantine_subdirs())
        .unwrap_or_default();
    quarantine_findings_in(dir, subdirs, limit)
}

/// [`doctor_in_with`] over `host`, resolving homes against this test process's
/// environment.
#[cfg(test)]
pub(super) fn doctor_for_with(
    host: &RuntimeHost,
    params: IntegrationDoctorParams,
    python: &[IntegrationFinding],
    socket: &[IntegrationFinding],
) -> Result<IntegrationDoctorResult, ProtocolError> {
    doctor_in_with(&super::config_homes_for_tests(host), params, python, socket)
}

/// [`doctor_in`] with the runtime probes supplied by the caller.
pub(super) fn doctor_in_with(
    homes: &ConfigHomes,
    params: IntegrationDoctorParams,
    python: &[IntegrationFinding],
    socket: &[IntegrationFinding],
) -> Result<IntegrationDoctorResult, ProtocolError> {
    let IntegrationDoctorParams {
        agent,
        profile,
        all_profiles,
    } = params;
    let selection = HomeSelection::from_params(profile, all_profiles)?;
    let selected = homes.targets(agent.as_ref(), &selection)?;
    let runtime: Vec<IntegrationFinding> = python.iter().chain(socket).cloned().collect();
    let agents: Vec<IntegrationAgentDoctor> = selected
        .iter()
        .map(|target| {
            let mut diagnosis = diagnose_target(target, &runtime);
            diagnosis.home.clone_from(&target.label);
            diagnosis
        })
        .collect();
    Ok(IntegrationDoctorResult {
        ok: agents.iter().all(|agent| agent.ok),
        agents,
        home_selectors: true,
    })
}

/// Diagnoses the home of `target`.
fn diagnose_target(target: &Target, runtime: &[IntegrationFinding]) -> IntegrationAgentDoctor {
    let agent = &target.resolved;
    let config_dir = target.dir_ref().ok();
    let probe = config_dir.map_or(LockProbe::Absent, probe_installer_lock);
    if matches!(probe, LockProbe::Busy) {
        return operation_in_progress(agent.runtime.clone());
    }
    super::commit::race_point("doctor.inspecting", agent.runtime.as_wire());
    let mut diagnosis = diagnose_as(
        agent.handler.inspect_provider(target.dir_ref()),
        agent.runtime.as_wire(),
        runtime,
    );
    retarget_diagnosis(&mut diagnosis, &agent.runtime);
    scope_diagnosis(&mut diagnosis, target);
    if let Some(dir) = config_dir {
        diagnosis.findings.extend(quarantine_findings_in(
            dir,
            agent.handler.quarantine_subdirs(),
            MAX_SCANNED_QUARANTINE_ENTRIES,
        ));
        if matches!(probe, LockProbe::Unsafe) {
            diagnosis.findings.push(unsafe_installer_lock(dir));
        }
        diagnosis.ok = diagnosis
            .findings
            .iter()
            .all(|finding| finding.severity != IntegrationFindingSeverity::Error);
    }
    // Without a lock file to hold, an operation that created it during
    // the inspection may have parked originals under the same names.
    let started_meanwhile = matches!(probe, LockProbe::Absent)
        && config_dir.is_some_and(|dir| {
            !matches!(
                probe_installer_lock(dir),
                LockProbe::Absent | LockProbe::Unsafe
            )
        });
    if started_meanwhile {
        return operation_in_progress(agent.runtime.clone());
    }
    drop(probe);
    diagnosis
}

/// Rewrites the recovery commands of `diagnosis` to address the profile home
/// `target` is.
fn scope_diagnosis(diagnosis: &mut IntegrationAgentDoctor, target: &Target) {
    let Some(profile) = target.scope_profile() else {
        return;
    };
    let runtime = target.resolved.runtime.as_wire();
    if let Some(status) = &mut diagnosis.status {
        handler::scope_status(status, target);
    }
    for finding in &mut diagnosis.findings {
        finding.summary = handler::scope_text(&finding.summary, runtime, profile);
        finding.remediation = finding
            .remediation
            .as_deref()
            .map(|remediation| handler::scope_text(remediation, runtime, profile));
    }
}
