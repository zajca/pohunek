//! Live host-capability snapshot builder for `host.inspect`.
//!
//! Builds a [`HostCapabilities`] describing what this host can do right now:
//! which protocol version the daemon speaks, which agent kinds it supports,
//! which agent runtimes are actually installed (probed against `PATH`), and
//! whether git-backed worktree sessions are available. The snapshot is built
//! fresh on every request, so it always reflects the host as it is now and is
//! never cached.

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use protocol::{
    AgentRuntime, HostCapabilities, ProtocolError, RuntimeId, RuntimeRef, PROTOCOL_VERSION,
};
use serde::Deserialize;

use crate::agent::host::{
    LaunchProgram, RuntimeDefinition, RuntimeRegistry, VersionProbePolicy, RESERVED_RUNTIME_IDS,
    SEMVER_PARSER_ID,
};
use crate::agent::{which_executable, ProfileRegistry, ValidatedLaunchProgram};

/// The reviewed Hermes release metadata shipped with this Pohunek build.
const HERMES_COMPATIBILITY_LOCK: &str =
    include_str!("../../../compat/hermes/compatibility-lock.json");
/// Maximum wall time for a local version probe, including interpreter startup.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Maximum stdout the version probe may write or retain.
const VERSION_OUTPUT_LIMIT: usize = 4 * 1024;
/// Maximum normalized version length accepted from untrusted probe output.
const MAX_HERMES_VERSION_BYTES: usize = 64;
/// Poll cadence balances bounded shutdown with negligible idle CPU use.
const VERSION_PROBE_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// A deterministic executable search path avoids inheriting user shims or hooks.
const HERMES_PROBE_PATH: &str = "/usr/bin:/bin";
/// A deterministic locale keeps provider version output stable.
const PROBE_LOCALE: &str = "C";

/// Version-probe parsers the daemon compiles in, selected by the parser id a
/// runtime definition names under `version_probe.parser`.
///
/// Each parser owns the probe sandbox, the output grammar and the versions the
/// daemon accepts for launch. The Hermes lock is core data until the Hermes
/// package owns its own compatibility policy; the data-driven parser takes its
/// arguments and release range from the runtime definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VersionProbe<'a> {
    /// Parser id `hermes-v1`: `hermes --version` pinned to the checked-in
    /// compatibility lock release.
    HermesV1,
    /// Parser id `semver-v1`: the declared arguments print one
    /// `MAJOR.MINOR.PATCH` release that must lie in the declared range.
    SemverV1(&'a VersionProbePolicy),
}

impl<'a> VersionProbe<'a> {
    /// Resolves a definition's parser id, or `None` for an id this daemon does
    /// not compile in or a data-driven parser without its policy.
    fn for_definition(definition: &'a RuntimeDefinition) -> Option<Self> {
        match definition.version_probe_parser()?.as_str() {
            "hermes-v1" => Some(Self::HermesV1),
            SEMVER_PARSER_ID => definition.version_probe_policy().map(Self::SemverV1),
            _ => None,
        }
    }

    /// Runs the probe against `path` and returns the normalized version.
    fn detect_version(self, path: &Path) -> Option<String> {
        match self {
            Self::HermesV1 => {
                run_hermes_version_probe(path).and_then(|output| parse_hermes_version(&output))
            }
            Self::SemverV1(policy) => run_semver_version_probe(path, policy).and_then(|output| {
                VersionProbePolicy::parse_output(&output).map(|version| version.to_string())
            }),
        }
    }

    /// Whether this daemon accepts `version` for launch.
    fn accepts(self, version: &str) -> bool {
        match self {
            Self::HermesV1 => version == supported_hermes_version(),
            Self::SemverV1(policy) => crate::agent::host::ProbeVersion::parse(version)
                .is_some_and(|version| policy.supports(version)),
        }
    }
}

/// Resolves the definition a base runtime launches, when one is registered.
#[cfg(test)]
fn definition_for_base<'a>(
    registry: &'a RuntimeRegistry,
    base: &RuntimeId,
) -> Option<&'a Arc<RuntimeDefinition>> {
    registry.resolve(base).ok()
}

/// Position of a runtime id in the reported inventory: the reserved built-ins
/// keep their documented order, every other runtime follows in id order.
fn inventory_rank(runtime_id: &RuntimeId) -> usize {
    RESERVED_RUNTIME_IDS
        .iter()
        .position(|reserved| *reserved == runtime_id.as_str())
        .unwrap_or(RESERVED_RUNTIME_IDS.len())
}

/// Build the live capability snapshot for this host.
///
/// `supported_agents` is every built-in runtime in the registry plus every
/// resolvable host agent profile (Part C). `runtimes` reports, per agent, whether
/// its backing program is present on `PATH`: a runtime launched through the
/// host's login shell is always available (no path), and every other runtime
/// probes its (possibly-overridden) program. A runtime whose definition names a
/// version-probe parser additionally reports its version and whether the daemon
/// accepts it. Probing uses the same executable check as the launch path
/// ([`which_executable`]) so "available" agrees with what a launch would accept.
/// `git_available` reflects a `git` probe and currently also gates worktree support.
#[must_use]
pub(crate) fn host_capabilities(
    daemon_version: &str,
    profiles: &ProfileRegistry,
) -> HostCapabilities {
    host_capabilities_for(daemon_version, profiles, &profiles.runtimes().registry())
}

/// [`host_capabilities`] against an explicit runtime registry.
fn host_capabilities_for(
    daemon_version: &str,
    profiles: &ProfileRegistry,
    registry: &RuntimeRegistry,
) -> HostCapabilities {
    let mut definitions: Vec<&Arc<RuntimeDefinition>> = registry.definitions().collect();
    // The sort is stable, so runtimes outside the reserved set stay in id order.
    definitions.sort_by_key(|definition| inventory_rank(definition.runtime_id()));

    let mut supported_agents = Vec::with_capacity(definitions.len());
    let mut runtimes = Vec::with_capacity(definitions.len());
    for definition in definitions {
        let agent = definition.runtime_id().as_str();
        let agent_base = RuntimeRef::from(definition.runtime_id().clone());
        runtimes.push(match definition.program() {
            // A host-shell runtime needs no named binary on PATH.
            LaunchProgram::HostShell(_) => AgentRuntime {
                agent: agent.to_owned(),
                agent_base: Some(agent_base),
                available: true,
                path: None,
                version: None,
                supported: None,
            },
            LaunchProgram::Fixed(program) => {
                probe_definition(agent, agent_base, definition, program)
            }
        });
        supported_agents.push(agent.to_owned());
    }

    // Host profiles: each resolvable profile is a launchable agent name; probe its
    // resolved program exactly as its base runtime, so availability is consistent.
    for agent in profiles.enumerate() {
        // A pinned profile is described by the definition it launches from,
        // and is unavailable once that package cannot launch a fresh session.
        let launchable = profiles.runtimes().verify_launchable(&agent.definition);
        let base = RuntimeRef::from(agent.base.clone());
        let runtime = match (&launchable, agent.profile.as_ref()) {
            (Err(_), _) => unavailable_runtime(&agent.name, base),
            (Ok(()), Some(profile)) => {
                probe_definition(&agent.name, base, &agent.definition, &profile.program)
            }
            (Ok(()), None) => probe_definition(
                &agent.name,
                base,
                &agent.definition,
                agent.definition.program().as_str(),
            ),
        };
        runtimes.push(runtime);
        supported_agents.push(agent.name);
    }
    for profile in profiles.unavailable_pinned() {
        runtimes.push(unavailable_runtime(
            &profile.name,
            RuntimeRef::from(profile.base),
        ));
        supported_agents.push(profile.name);
    }

    let git_available = which_on_path("git").is_some();

    HostCapabilities {
        daemon_version: daemon_version.to_owned(),
        protocol_version: PROTOCOL_VERSION,
        supported_agents,
        runtimes,
        git_available,
        // Worktree-per-session is implemented on top of `git worktree`, so its
        // availability currently follows git's presence on the host.
        worktree_supported: git_available,
        terminal_read_supported: true,
        output_read_supported: true,
        session_wait_supported: true,
    }
}

/// The entry of an agent name that has nothing to launch right now.
fn unavailable_runtime(agent: &str, agent_base: RuntimeRef) -> AgentRuntime {
    AgentRuntime {
        agent: agent.to_owned(),
        agent_base: Some(agent_base),
        available: false,
        path: None,
        version: None,
        supported: None,
    }
}

/// Probe `program` for `definition`, honouring its version-probe parser.
fn probe_definition(
    agent: &str,
    agent_base: RuntimeRef,
    definition: &RuntimeDefinition,
    program: &str,
) -> AgentRuntime {
    match definition.version_probe_parser() {
        Some(_) => probe_versioned_runtime(agent, agent_base, program, definition),
        None => probe_runtime(agent, agent_base, program),
    }
}

/// Probe `PATH` for an agent's backing program and build its runtime entry.
///
/// `available` is true exactly when the program resolves to an **executable** on
/// `PATH` (the launch-path check, [`which_executable`]); the resolved path is
/// reported when found.
fn probe_runtime(agent: &str, agent_base: RuntimeRef, binary: &str) -> AgentRuntime {
    match which_executable(binary) {
        Some(path) => AgentRuntime {
            agent: agent.to_owned(),
            agent_base: Some(agent_base),
            available: true,
            path: Some(path.display().to_string()),
            version: None,
            supported: None,
        },
        None => AgentRuntime {
            agent: agent.to_owned(),
            agent_base: Some(agent_base),
            available: false,
            path: None,
            version: None,
            supported: None,
        },
    }
}

/// Probe an executable with the compiled parser `definition` names and
/// classify its version without exposing output.
///
/// A present runtime always reports `supported`: `Some(true)` only for an
/// accepted version, `Some(false)` for any other version, unparseable output or
/// a parser id this daemon does not compile in.
fn probe_versioned_runtime(
    agent: &str,
    agent_base: RuntimeRef,
    binary: &str,
    definition: &RuntimeDefinition,
) -> AgentRuntime {
    let Some(program) = ValidatedLaunchProgram::resolve(binary) else {
        return AgentRuntime {
            agent: agent.to_owned(),
            agent_base: Some(agent_base),
            available: false,
            path: None,
            version: None,
            supported: None,
        };
    };

    let probe = VersionProbe::for_definition(definition);
    let version = probe.and_then(|probe| probe.detect_version(program.as_path()));
    let supported = probe
        .zip(version.as_deref())
        .is_some_and(|(probe, version)| probe.accepts(version));
    AgentRuntime {
        agent: agent.to_owned(),
        agent_base: Some(agent_base),
        available: true,
        path: Some(program.as_path().display().to_string()),
        version,
        supported: Some(supported),
    }
}

/// Validates a launch runtime and pins the resolved executable path when its
/// definition names a version-probe parser.
///
/// Runtimes without a version-probe parser keep plain launch behavior. A
/// runtime with one resolves the executable once, probes it in the same
/// isolated bounded sandbox used by inventory, and returns that exact path only
/// for the pinned supported release. A parser id this daemon does not compile in
/// refuses the launch. Probe output, configured paths, and detected versions
/// never enter the error.
#[cfg(test)]
pub(crate) fn validate_launch_runtime(
    base: &RuntimeRef,
    binary: &str,
) -> Result<Option<ValidatedLaunchProgram>, ProtocolError> {
    let registry = crate::agent::host::RuntimeHost::default().registry();
    match base.id().and_then(|id| definition_for_base(&registry, id)) {
        Some(definition) => validate_definition_launch(definition, binary),
        None => Ok(None),
    }
}

/// [`validate_launch_runtime`] for an already resolved definition.
///
/// # Errors
///
/// Returns `agent_runtime_unsupported` when the probe rejects the executable
/// or the definition names a parser this daemon does not provide.
pub(crate) fn validate_definition_launch(
    definition: &RuntimeDefinition,
    binary: &str,
) -> Result<Option<ValidatedLaunchProgram>, ProtocolError> {
    if definition.version_probe_parser().is_none() {
        return Ok(None);
    }

    let program = ValidatedLaunchProgram::resolve(binary)
        .ok_or_else(ProtocolError::agent_runtime_unsupported)?;
    let supported = VersionProbe::for_definition(definition).is_some_and(|probe| {
        probe
            .detect_version(program.as_path())
            .is_some_and(|version| probe.accepts(&version))
    });
    supported
        .then_some(Some(program))
        .ok_or_else(ProtocolError::agent_runtime_unsupported)
}

/// Minimal view of the checked-in compatibility lock needed at runtime.
#[derive(Deserialize)]
struct HermesCompatibilityLock {
    release: String,
}

fn supported_hermes_version() -> &'static str {
    static RELEASE: OnceLock<String> = OnceLock::new();
    RELEASE
        .get_or_init(|| {
            serde_json::from_str::<HermesCompatibilityLock>(HERMES_COMPATIBILITY_LOCK)
                .expect("embedded Hermes compatibility lock must be valid JSON")
                .release
        })
        .as_str()
}

/// Run `--version` with isolated Hermes state, bounded time, and bounded output.
fn run_hermes_version_probe(path: &std::path::Path) -> Option<String> {
    run_hermes_version_probe_with_timeout(path, VERSION_PROBE_TIMEOUT)
}

fn run_hermes_version_probe_with_timeout(
    path: &std::path::Path,
    timeout: Duration,
) -> Option<String> {
    run_hermes_version_probe_with_seeded_env(path, timeout, &[])
}

fn run_hermes_version_probe_with_seeded_env(
    path: &std::path::Path,
    timeout: Duration,
    seeded_env: &[(&str, &str)],
) -> Option<String> {
    match probe_hermes_version(path, timeout, seeded_env) {
        ProbeOutcome::Output(output) => Some(output),
        ProbeOutcome::Failed | ProbeOutcome::TimedOut | ProbeOutcome::Unavailable => None,
    }
}

/// How a version probe ended.
///
/// Callers that only need the version text collapse every non-`Output` case to
/// `None`; the distinction lets tests tell the output-limit termination apart
/// from the probe's own deadline.
#[derive(Debug)]
enum ProbeOutcome {
    /// The child exited successfully; carries the retained stdout.
    Output(String),
    /// The child exited unsuccessfully, including termination by a signal such
    /// as the `SIGXFSZ` raised when it exceeds the output-file size limit.
    Failed,
    /// The deadline expired and the child's process group was terminated.
    TimedOut,
    /// The probe could not be set up, polled or read.
    Unavailable,
}

fn probe_hermes_version(
    path: &std::path::Path,
    timeout: Duration,
    seeded_env: &[(&str, &str)],
) -> ProbeOutcome {
    probe_hermes_version_with_clock(path, timeout, seeded_env, Instant::now)
}

/// [`probe_hermes_version`] reading time from `now`.
///
/// The clock drives only the deadline: it is read once to start the deadline and
/// once per poll of the child. Tests inject a clock to prove the probe honors
/// its `timeout` argument without measuring real time.
fn probe_hermes_version_with_clock(
    path: &std::path::Path,
    timeout: Duration,
    seeded_env: &[(&str, &str)],
    now: impl Fn() -> Instant,
) -> ProbeOutcome {
    spawn_and_collect_probe(path, ProbeKind::Pinned, timeout, seeded_env, &now)
        .unwrap_or(ProbeOutcome::Unavailable)
}

/// Runs the data-driven probe `policy` declares with isolated state, bounded
/// time and bounded output, and returns its standard output.
fn run_semver_version_probe(path: &Path, policy: &VersionProbePolicy) -> Option<String> {
    match spawn_and_collect_probe(
        path,
        ProbeKind::Semver(policy),
        VERSION_PROBE_TIMEOUT,
        &[],
        &Instant::now,
    )? {
        ProbeOutcome::Output(output) => Some(output),
        ProbeOutcome::Failed | ProbeOutcome::TimedOut | ProbeOutcome::Unavailable => None,
    }
}

/// Which probe `spawn_and_collect_probe` runs: it fixes the arguments and the
/// environment the child sees.
#[derive(Clone, Copy)]
enum ProbeKind<'a> {
    /// `--version` under the fixed search path of the compiled probe.
    Pinned,
    /// The declared arguments under a runtime-neutral environment.
    Semver(&'a VersionProbePolicy),
}

/// The environment variable that holds the executable search path.
const SEARCH_PATH_VAR: &str = "PATH";

fn spawn_and_collect_probe(
    path: &std::path::Path,
    kind: ProbeKind<'_>,
    timeout: Duration,
    seeded_env: &[(&str, &str)],
    now: &impl Fn() -> Instant,
) -> Option<ProbeOutcome> {
    let sandbox = VersionProbeSandbox::create()?;
    let output_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&sandbox.output_path)
        .ok()?;
    set_owner_only_file_permissions(&output_file).ok()?;
    let mut command = Command::new(path);
    command.envs(seeded_env.iter().copied());
    command
        .env_clear()
        .current_dir(&sandbox.cwd_path)
        .env("LANG", PROBE_LOCALE)
        .env("LC_ALL", PROBE_LOCALE)
        .env("HOME", &sandbox.home_path)
        .env("XDG_CONFIG_HOME", &sandbox.xdg_config_path)
        .env("XDG_CACHE_HOME", &sandbox.xdg_cache_path)
        .env("XDG_DATA_HOME", &sandbox.xdg_data_path)
        .env("XDG_STATE_HOME", &sandbox.xdg_state_path)
        .env("XDG_RUNTIME_DIR", &sandbox.xdg_runtime_path)
        .env("TMPDIR", &sandbox.tmp_path);
    match kind {
        ProbeKind::Pinned => {
            command
                .arg("--version")
                .env(SEARCH_PATH_VAR, HERMES_PROBE_PATH)
                .env("HERMES_HOME", &sandbox.hermes_home_path)
                .env("PYTHONNOUSERSITE", "1")
                .env("PYTHONDONTWRITEBYTECODE", "1")
                .env("PYTHONSAFEPATH", "1")
                .env("PYTHONPYCACHEPREFIX", &sandbox.python_cache_path);
        }
        ProbeKind::Semver(policy) => {
            // The probed program may be an interpreter script (`#!/usr/bin/env
            // node`); the search path the daemon launches agents with finds the
            // same interpreter the launch will use.
            command.args(policy.args());
            if let Some(search_path) = std::env::var_os(SEARCH_PATH_VAR) {
                command.env(SEARCH_PATH_VAR, search_path);
            }
        }
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(output_file))
        .stderr(Stdio::null());
    configure_probe_process_group(&mut command);

    let mut child = command.spawn().ok()?;
    let deadline = now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if now() < deadline => {
                std::thread::sleep(VERSION_PROBE_POLL_INTERVAL);
            }
            Ok(None) => {
                terminate_probe_process(&mut child);
                return Some(ProbeOutcome::TimedOut);
            }
            Err(_) => {
                terminate_probe_process(&mut child);
                return Some(ProbeOutcome::Unavailable);
            }
        }
    };
    terminate_probe_process_group(child.id());
    let mut output = Vec::with_capacity(VERSION_OUTPUT_LIMIT);
    File::open(&sandbox.output_path)
        .ok()?
        .take(u64::try_from(VERSION_OUTPUT_LIMIT).ok()?)
        .read_to_end(&mut output)
        .ok()?;
    Some(if status.success() {
        ProbeOutcome::Output(String::from_utf8_lossy(&output).into_owned())
    } else {
        ProbeOutcome::Failed
    })
}

/// Owner-private state and output paths for one version probe.
#[derive(Debug)]
struct VersionProbeSandbox {
    dir: PathBuf,
    home_path: PathBuf,
    hermes_home_path: PathBuf,
    xdg_config_path: PathBuf,
    xdg_cache_path: PathBuf,
    xdg_data_path: PathBuf,
    xdg_state_path: PathBuf,
    xdg_runtime_path: PathBuf,
    tmp_path: PathBuf,
    python_cache_path: PathBuf,
    cwd_path: PathBuf,
    output_path: PathBuf,
}

impl VersionProbeSandbox {
    fn create() -> Option<Self> {
        for _ in 0..4 {
            let dir =
                std::env::temp_dir().join(format!("pohunek-version-probe-{}", ulid::Ulid::new()));
            match create_owner_only_dir(&dir) {
                Ok(()) => {
                    let sandbox = Self {
                        home_path: dir.join("home"),
                        hermes_home_path: dir.join("hermes-home"),
                        xdg_config_path: dir.join("xdg-config"),
                        xdg_cache_path: dir.join("xdg-cache"),
                        xdg_data_path: dir.join("xdg-data"),
                        xdg_state_path: dir.join("xdg-state"),
                        xdg_runtime_path: dir.join("xdg-runtime"),
                        tmp_path: dir.join("tmp"),
                        python_cache_path: dir.join("python-cache"),
                        cwd_path: dir.join("cwd"),
                        output_path: dir.join("stdout"),
                        dir,
                    };
                    if sandbox
                        .private_dirs()
                        .iter()
                        .try_for_each(|path| create_owner_only_dir(path))
                        .is_err()
                    {
                        drop(sandbox);
                        return None;
                    }
                    return Some(sandbox);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return None,
            }
        }
        None
    }

    fn private_dirs(&self) -> [&Path; 10] {
        [
            &self.home_path,
            &self.hermes_home_path,
            &self.xdg_config_path,
            &self.xdg_cache_path,
            &self.xdg_data_path,
            &self.xdg_state_path,
            &self.xdg_runtime_path,
            &self.tmp_path,
            &self.python_cache_path,
            &self.cwd_path,
        ]
    }
}

impl Drop for VersionProbeSandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(unix)]
fn create_owner_only_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    std::fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_owner_only_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir(path)
}

#[cfg(unix)]
fn set_owner_only_file_permissions(file: &File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_owner_only_file_permissions(_file: &File) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn configure_probe_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
    #[expect(
        unsafe_code,
        reason = "pre_exec is required to cap an untrusted version probe's output file"
    )]
    // SAFETY: setrlimit is async-signal-safe and the closure only constructs a
    // fixed rlimit value before calling it. An error prevents the child exec.
    unsafe {
        command.pre_exec(|| {
            let output_limit = libc::rlimit {
                rlim_cur: VERSION_OUTPUT_LIMIT as libc::rlim_t,
                rlim_max: VERSION_OUTPUT_LIMIT as libc::rlim_t,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &raw const output_limit) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}

#[cfg(not(unix))]
fn configure_probe_process_group(_command: &mut Command) {}

fn terminate_probe_process(child: &mut std::process::Child) {
    terminate_probe_process_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

fn terminate_probe_process_group(child_id: u32) {
    #[cfg(unix)]
    {
        let Ok(pid) = i32::try_from(child_id) else {
            return;
        };
        #[expect(
            unsafe_code,
            reason = "libc::kill is required to terminate a timed-out probe subtree"
        )]
        // SAFETY: the child starts in a process group whose id is its positive
        // pid; negating it targets only that group.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child_id;
}

/// Parse the first canonical `Hermes Agent v<semver>` line.
fn parse_hermes_version(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let token = line
            .trim()
            .strip_prefix("Hermes Agent v")?
            .split_whitespace()
            .next()?;
        is_normalized_semver(token).then(|| token.to_owned())
    })
}

fn is_normalized_semver(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_HERMES_VERSION_BYTES {
        return false;
    }

    let (without_build, build) = value
        .split_once('+')
        .map_or((value, None), |(version, build)| (version, Some(build)));
    if build.is_some_and(|build| !valid_semver_identifiers(build, false)) {
        return false;
    }
    let (core, prerelease) = without_build
        .split_once('-')
        .map_or((without_build, None), |(core, prerelease)| {
            (core, Some(prerelease))
        });
    if prerelease.is_some_and(|prerelease| !valid_semver_identifiers(prerelease, true)) {
        return false;
    }

    let mut parts = core.split('.');
    (0..3).all(|_| {
        parts.next().is_some_and(|part| {
            !part.is_empty()
                && (part == "0" || !part.starts_with('0'))
                && part.bytes().all(|byte| byte.is_ascii_digit())
        })
    }) && parts.next().is_none()
}

fn valid_semver_identifiers(value: &str, reject_numeric_leading_zero: bool) -> bool {
    !value.is_empty()
        && value.split('.').all(|identifier| {
            !identifier.is_empty()
                && identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && !(reject_numeric_leading_zero
                    && identifier.len() > 1
                    && identifier.starts_with('0')
                    && identifier.bytes().all(|byte| byte.is_ascii_digit()))
        })
}

/// Resolve a binary name against the `PATH` environment variable.
///
/// A small dependency-free `which`: splits `PATH`, joins the name, and returns
/// the first executable file. Mirrors the agent launch-path probe rather than
/// reporting a non-executable placeholder as available.
fn which_on_path(name: &str) -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    which_on_path_value(name, &path_var)
}

fn which_on_path_value(name: &str, path_var: &OsStr) -> Option<std::path::PathBuf> {
    hostcheck::resolve_executable(name, Some(path_var))
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::os::unix::fs::PermissionsExt;

    use pohunek_test_support::wait::{poll_until, HANG_GUARD};

    use super::*;
    use crate::agent::host::{BuiltinSource, DefinitionOrigin};
    use crate::procwatch::{HostInspector, Pid, ProcessFact, ProcessIdentity, ProcessInspector};

    /// How long fixture processes block; well beyond `HANG_GUARD` so a fixture
    /// can only end through the probe, never by exiting on its own.
    const FIXTURE_HOLD: Duration = Duration::from_secs(HANG_GUARD.as_secs() * 5);

    /// An empty profile registry (no host-config layer) for the base-kind tests.
    fn no_profiles() -> ProfileRegistry {
        ProfileRegistry::default()
    }

    fn temp_agents_dir() -> crate::test_support::ScopedDir {
        crate::test_support::scoped_dir("pohunek-caps-")
    }

    #[test]
    fn snapshot_reports_protocol_version_and_four_supported_agents() {
        let caps = host_capabilities("1.2.3-test", &no_profiles());
        assert_eq!(caps.daemon_version, "1.2.3-test");
        assert_eq!(caps.protocol_version, PROTOCOL_VERSION);
        assert_eq!(
            caps.supported_agents,
            vec!["shell", "codex", "claude", "hermes"]
        );
        // worktree support tracks git availability.
        assert_eq!(caps.worktree_supported, caps.git_available);
    }

    #[test]
    fn shell_runtime_is_always_available() {
        let caps = host_capabilities("0.0.0", &no_profiles());
        let shell = caps
            .runtimes
            .iter()
            .find(|runtime| runtime.agent == "shell")
            .expect("shell runtime is reported");
        assert!(shell.available, "shell runtime must always be available");
        assert!(
            shell.path.is_none(),
            "shell runtime carries no resolved path"
        );
        assert_eq!(shell.agent_base, Some(RuntimeRef::shell()));
    }

    #[test]
    fn agent_runtime_availability_matches_resolved_path() {
        let caps = host_capabilities("0.0.0", &no_profiles());
        for runtime in &caps.runtimes {
            if runtime.agent == "shell" {
                continue;
            }
            // For probed agents, availability is exactly path presence.
            assert_eq!(runtime.available, runtime.path.is_some());
        }
    }

    #[test]
    fn host_capabilities_enumerates_resolvable_profiles_and_probes_their_program() {
        let dir = temp_agents_dir();
        // A profile whose program is a real executable so availability is true.
        std::fs::write(
            dir.join("my-claude.toml"),
            "base = \"claude\"\nprogram = \"/bin/sh\"\n",
        )
        .expect("write profile");
        let caps = host_capabilities("0.0.0", &ProfileRegistry::new(Some(dir.clone())));

        assert!(
            caps.supported_agents.contains(&"my-claude".to_owned()),
            "a resolvable profile is a supported agent: {:?}",
            caps.supported_agents
        );
        let runtime = caps
            .runtimes
            .iter()
            .find(|runtime| runtime.agent == "my-claude")
            .expect("profile runtime is probed");
        // The availability invariant holds for profile programs too.
        assert_eq!(runtime.available, runtime.path.is_some());
        assert!(runtime.available, "/bin/sh resolves as executable");
        assert_eq!(runtime.agent_base, Some(RuntimeRef::claude()));
    }

    #[test]
    fn path_probe_ignores_non_executable_files() {
        let dir = temp_agents_dir();
        let git = dir.join("git");
        std::fs::write(&git, "#!/bin/sh\n").expect("write fake git");
        let mut perms = std::fs::metadata(&git).expect("metadata").permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&git, perms).expect("chmod fake git");

        assert!(
            which_on_path_value("git", dir.as_os_str()).is_none(),
            "capability probing must match launch probing and require executable files"
        );
    }

    /// A package-served definition whose `[runtime]` table carries `probe` as
    /// its `version_probe` value.
    fn probed_definition(probe: &str) -> RuntimeDefinition {
        let document = format!(
            r#"schema = 1
id = "acme.runtime.tool"
version = "1.0.0"
runtime_api = 1

[runtime]
id = "tool"
name = "Tool"
program = "tool"
args = []
detect_manifest = "any"
version_probe = {probe}

[input]
bracketed_paste = false
submit_delay_ms = 0
text_policy = "unrestricted"

[resume]
supported = false

[fork]
supported = false

[native_reference]
strategy = "none"
"#
        );
        RuntimeDefinition::from_toml(
            &document,
            |package| DefinitionOrigin::Package {
                package,
                digest: protocol::PackageDigest::parse(&format!("sha256:{}", "3".repeat(64)))
                    .expect("digest"),
            },
            |_| Ok(Arc::new(crate::detect::generic_shell_manifest().clone())),
        )
        .expect("valid definition")
    }

    fn hermes_definition() -> Arc<RuntimeDefinition> {
        let registry = test_registry();
        Arc::clone(
            registry
                .resolve(&RuntimeId::hermes())
                .expect("hermes registered"),
        )
    }

    fn probe_hermes_for_test(binary: &str) -> AgentRuntime {
        probe_versioned_runtime("hermes", RuntimeRef::hermes(), binary, &hermes_definition())
    }

    const SEMVER_PROBE: &str =
        r#"{ parser = "semver-v1", args = ["--version"], min = "1.0.0", below = "1.1.0" }"#;

    /// Registry whose shell runtime launches a fixed program, so the
    /// inventory does not depend on the host's `$SHELL`.
    fn test_registry() -> RuntimeRegistry {
        RuntimeRegistry::from_sources(&[&BuiltinSource::new("/bin/sh")])
            .expect("built-in registry builds")
    }

    #[test]
    fn invalid_login_shell_never_breaks_the_registry_or_non_shell_runtimes() {
        let oversized = "s".repeat(crate::agent::host::MAX_ARG_BYTES + 1);
        for shell in [
            Some(String::new()),
            Some(oversized),
            Some("/bin/s\nh".to_owned()),
            Some("/bin/s\u{7f}h".to_owned()),
            None,
        ] {
            let registry =
                RuntimeRegistry::from_sources(&[&BuiltinSource::from_login_shell(shell)])
                    .expect("an invalid login shell must not fail the registry");
            let caps = host_capabilities_for("0.0.0", &no_profiles(), &registry);
            let names: Vec<&str> = caps.runtimes.iter().map(|r| r.agent.as_str()).collect();
            assert_eq!(names, RESERVED_RUNTIME_IDS);
            let shell_def = registry
                .resolve(&RuntimeId::parse("shell").expect("id"))
                .expect("shell registered");
            assert_eq!(shell_def.program().as_str(), "/bin/sh");
            assert!(definition_for_base(&registry, &RuntimeId::hermes()).is_some());
        }
        let valid = BuiltinSource::from_login_shell(Some("/usr/bin/zsh".to_owned()));
        let registry = RuntimeRegistry::from_sources(&[&valid]).expect("registry");
        let shell_def = registry
            .resolve(&RuntimeId::parse("shell").expect("id"))
            .expect("shell registered");
        assert_eq!(shell_def.program().as_str(), "/usr/bin/zsh");
    }

    #[test]
    fn inventory_lists_builtins_in_reserved_order_with_their_wire_shape() {
        let caps = host_capabilities_for("0.0.0", &no_profiles(), &test_registry());
        let order: Vec<&str> = caps.runtimes.iter().map(|r| r.agent.as_str()).collect();
        assert_eq!(order, RESERVED_RUNTIME_IDS);
        assert_eq!(caps.supported_agents, RESERVED_RUNTIME_IDS);

        let bases: Vec<RuntimeRef> = caps
            .runtimes
            .iter()
            .map(|r| r.agent_base.clone().expect("base reported"))
            .collect();
        assert_eq!(
            bases,
            [
                RuntimeRef::shell(),
                RuntimeRef::codex(),
                RuntimeRef::claude(),
                RuntimeRef::hermes()
            ]
        );
        // Only the runtime with a version-probe parser reports a policy.
        for runtime in &caps.runtimes {
            assert_eq!(
                runtime.supported.is_some(),
                runtime.agent == "hermes" && runtime.available,
                "{}",
                runtime.agent
            );
        }
    }

    #[test]
    fn builtin_definitions_name_only_compiled_version_probes() {
        let host = crate::agent::host::RuntimeHost::default();
        let registry = host.registry();
        for definition in registry.definitions() {
            if definition.version_probe_parser().is_some() {
                assert!(
                    VersionProbe::for_definition(definition).is_some(),
                    "{} names an uncompiled parser",
                    definition.runtime_id()
                );
            }
        }
        let registry = host.registry();
        let hermes = registry
            .resolve(&RuntimeId::parse("hermes").expect("id"))
            .expect("hermes registered");
        assert_eq!(
            hermes
                .version_probe_parser()
                .map(crate::agent::host::HandlerId::as_str),
            Some("hermes-v1")
        );
    }

    #[test]
    fn unknown_parser_id_reports_unsupported_in_inventory() {
        let dir = temp_agents_dir();
        let program = dir.join("tool");
        write_test_executable(&program, "#!/bin/sh\necho 'Hermes Agent v0.20.0'\n");
        let definition = probed_definition(r#"{ parser = "not-compiled" }"#);

        let runtime = probe_versioned_runtime(
            "custom",
            RuntimeRef::hermes(),
            &program.display().to_string(),
            &definition,
        );
        assert!(runtime.available);
        assert_eq!(runtime.version, None);
        assert_eq!(runtime.supported, Some(false));
    }

    #[test]
    fn profile_over_hermes_base_is_probed_with_the_hermes_parser() {
        let dir = temp_agents_dir();
        let hermes = dir.join("hermes-bin");
        write_test_executable(&hermes, "#!/bin/sh\necho 'Hermes Agent v0.21.0'\n");
        std::fs::write(
            dir.join("hermes-review.toml"),
            format!("base = \"hermes\"\nprogram = \"{}\"\n", hermes.display()),
        )
        .expect("write profile");
        let caps = host_capabilities_for(
            "0.0.0",
            &ProfileRegistry::new(Some(dir.clone())),
            &test_registry(),
        );
        let runtime = caps
            .runtimes
            .iter()
            .find(|runtime| runtime.agent == "hermes-review")
            .expect("profile runtime reported");
        assert_eq!(runtime.agent_base, Some(RuntimeRef::hermes()));
        assert_eq!(runtime.version.as_deref(), Some("0.21.0"));
        assert_eq!(runtime.supported, Some(false));
    }

    #[test]
    fn hermes_inventory_distinguishes_supported_unsupported_and_missing() {
        let dir = temp_agents_dir();
        let hermes = dir.join("hermes");

        write_test_executable(
            &hermes,
            "#!/bin/sh\necho 'Hermes Agent v0.20.0 (2026-08-03)'\n",
        );
        let supported = probe_hermes_for_test(&hermes.display().to_string());
        assert!(supported.available);
        assert_eq!(supported.agent_base, Some(RuntimeRef::hermes()));
        assert_eq!(supported.version.as_deref(), Some("0.20.0"));
        assert_eq!(supported.supported, Some(true));

        write_test_executable(&hermes, "#!/bin/sh\necho 'Hermes Agent v0.21.0 (future)'\n");
        let wrong = probe_hermes_for_test(&hermes.display().to_string());
        assert!(wrong.available);
        assert_eq!(wrong.version.as_deref(), Some("0.21.0"));
        assert_eq!(wrong.supported, Some(false));

        write_test_executable(&hermes, "#!/bin/sh\necho 'unexpected output'\n");
        let unparseable = probe_hermes_for_test(&hermes.display().to_string());
        assert!(unparseable.available);
        assert_eq!(unparseable.version, None);
        assert_eq!(unparseable.supported, Some(false));

        let missing = probe_hermes_for_test(&dir.join("missing").display().to_string());
        assert!(!missing.available);
        assert_eq!(missing.agent_base, Some(RuntimeRef::hermes()));
        assert_eq!(missing.path, None);
        assert_eq!(missing.version, None);
        assert_eq!(missing.supported, None);
    }

    #[test]
    fn hermes_launch_policy_accepts_only_the_pinned_runtime() {
        let dir = temp_agents_dir();
        let hermes = dir.join("hermes-wrapper");
        let binary = hermes.display().to_string();

        write_test_executable(&hermes, "#!/bin/sh\necho 'Hermes Agent v0.20.0'\n");
        let validated = validate_launch_runtime(&RuntimeRef::hermes(), &binary)
            .expect("pinned Hermes runtime")
            .expect("Hermes has a validated program");
        assert_eq!(
            validated.as_path(),
            hermes.canonicalize().expect("canonical Hermes fixture")
        );

        for (output, forbidden) in [
            ("Hermes Agent v0.21.0", "0.21.0"),
            ("unexpected output", "unexpected output"),
        ] {
            write_test_executable(&hermes, &format!("#!/bin/sh\necho '{output}'\n"));
            let error = validate_launch_runtime(&RuntimeRef::hermes(), &binary)
                .expect_err("incompatible Hermes runtime");
            assert_eq!(error.code, "agent_runtime_unsupported");
            assert!(!error.msg.contains(forbidden));
            assert!(!error.msg.contains(&binary));
        }

        let missing = dir.join("missing").display().to_string();
        let error = validate_launch_runtime(&RuntimeRef::hermes(), &missing)
            .expect_err("missing Hermes runtime");
        assert_eq!(error.code, "agent_runtime_unsupported");
        assert!(!error.msg.contains(&missing));

        assert_eq!(
            validate_launch_runtime(&RuntimeRef::claude(), &missing)
                .expect("non-Hermes behavior remains deferred to launch"),
            None
        );
    }

    #[test]
    fn hermes_version_probe_clears_ambient_state_and_isolates_all_writable_paths() {
        let dir = temp_agents_dir();
        let hermes = dir.join("hermes");
        let marker = dir.join("isolation.txt");
        write_test_executable(
            &hermes,
            &format!(
                "#!/bin/sh\n\
                 [ \"${{POHUNEK_PROBE_SENTINEL+x}}\" != x ] || exit 20\n\
                 [ \"${{PYTHONPATH+x}}\" != x ] || exit 21\n\
                 [ \"${{PYTHONHOME+x}}\" != x ] || exit 22\n\
                 [ \"${{VIRTUAL_ENV+x}}\" != x ] || exit 23\n\
                 [ \"${{CONDA_PREFIX+x}}\" != x ] || exit 24\n\
                 [ \"${{UV_PROJECT_ENVIRONMENT+x}}\" != x ] || exit 25\n\
                 [ \"$PATH\" = '{probe_path}' ] || exit 26\n\
                 [ \"$PYTHONNOUSERSITE\" = 1 ] || exit 27\n\
                 [ \"$PYTHONDONTWRITEBYTECODE\" = 1 ] || exit 28\n\
                 [ \"$PYTHONSAFEPATH\" = 1 ] || exit 29\n\
                 [ \"${{HOME##*/}}\" = home ] || exit 30\n\
                 [ \"${{HERMES_HOME##*/}}\" = hermes-home ] || exit 31\n\
                 [ \"${{XDG_CONFIG_HOME##*/}}\" = xdg-config ] || exit 32\n\
                 [ \"${{XDG_CACHE_HOME##*/}}\" = xdg-cache ] || exit 33\n\
                 [ \"${{XDG_DATA_HOME##*/}}\" = xdg-data ] || exit 34\n\
                 [ \"${{XDG_STATE_HOME##*/}}\" = xdg-state ] || exit 35\n\
                 [ \"${{XDG_RUNTIME_DIR##*/}}\" = xdg-runtime ] || exit 36\n\
                 [ \"${{TMPDIR##*/}}\" = tmp ] || exit 37\n\
                 [ \"${{PYTHONPYCACHEPREFIX##*/}}\" = python-cache ] || exit 38\n\
                 probe_cwd=$(pwd) || exit 39\n\
                 [ \"${{probe_cwd##*/}}\" = cwd ] || exit 40\n\
                 for private_dir in \"$HOME\" \"$HERMES_HOME\" \"$XDG_CONFIG_HOME\" \
                   \"$XDG_CACHE_HOME\" \"$XDG_DATA_HOME\" \"$XDG_STATE_HOME\" \
                   \"$XDG_RUNTIME_DIR\" \"$TMPDIR\" \"$PYTHONPYCACHEPREFIX\"; do\n\
                   [ -d \"$private_dir\" ] || exit 41\n\
                 done\n\
                 printf 'isolated' > {marker}\n\
                 echo 'Hermes Agent v0.20.0'\n",
                marker = marker.display(),
                probe_path = HERMES_PROBE_PATH,
            ),
        );

        let seeded_env = [
            ("PATH", "ambient-path-sentinel"),
            ("HOME", "ambient-home-sentinel"),
            ("HERMES_HOME", "ambient-hermes-sentinel"),
            ("XDG_CONFIG_HOME", "ambient-xdg-sentinel"),
            ("XDG_CACHE_HOME", "ambient-xdg-sentinel"),
            ("XDG_DATA_HOME", "ambient-xdg-sentinel"),
            ("XDG_STATE_HOME", "ambient-xdg-sentinel"),
            ("XDG_RUNTIME_DIR", "ambient-xdg-sentinel"),
            ("PYTHONPATH", "ambient-python-sentinel"),
            ("PYTHONHOME", "ambient-python-sentinel"),
            ("VIRTUAL_ENV", "ambient-python-sentinel"),
            ("CONDA_PREFIX", "ambient-python-sentinel"),
            ("UV_PROJECT_ENVIRONMENT", "ambient-python-sentinel"),
            ("POHUNEK_PROBE_SENTINEL", "present"),
        ];
        assert!(run_hermes_version_probe_with_seeded_env(
            &hermes,
            VERSION_PROBE_TIMEOUT,
            &seeded_env,
        )
        .is_some());
        assert_eq!(
            std::fs::read_to_string(marker).expect("isolation marker"),
            "isolated"
        );
    }

    #[test]
    fn hermes_version_probe_directories_are_owner_private() {
        let sandbox = VersionProbeSandbox::create().expect("create probe sandbox");
        for path in std::iter::once(sandbox.dir.as_path()).chain(sandbox.private_dirs()) {
            let mode = std::fs::metadata(path)
                .expect("private directory metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        let root = sandbox.dir.clone();
        drop(sandbox);
        assert!(!root.exists(), "probe sandbox must be removed on drop");
    }

    #[test]
    fn hermes_support_policy_comes_from_embedded_compatibility_lock() {
        let lock: HermesCompatibilityLock =
            serde_json::from_str(HERMES_COMPATIBILITY_LOCK).expect("parse compatibility lock");

        assert_eq!(supported_hermes_version(), lock.release);
        assert_eq!(supported_hermes_version(), "0.20.0");
    }

    #[test]
    fn hermes_version_parser_accepts_semver_and_rejects_malformed_tokens() {
        for valid in ["0.20.0", "0.21.0-rc.1", "1.0.0-0", "1.0.0-alpha-01+build.7"] {
            assert_eq!(
                parse_hermes_version(&format!("Hermes Agent v{valid}")),
                Some(valid.to_owned())
            );
        }
        for invalid in [
            "",
            "0.20",
            "00.20.0",
            "0.20.0-",
            "0.20.0+",
            "0.20.0-01",
            "1.0.0+one+two",
            "1.0.0-alpha..1",
            "1.0.0+build..1",
            "0.20.0..1",
            "0.20.0/evil",
        ] {
            assert_eq!(
                parse_hermes_version(&format!("Hermes Agent v{invalid}")),
                None,
                "unexpectedly accepted {invalid}"
            );
        }
    }

    #[test]
    fn hermes_version_probe_timeout_is_bounded() {
        let dir = temp_agents_dir();
        let hermes = dir.join("hermes");
        write_test_executable(
            &hermes,
            &format!("#!/bin/sh\nsleep {}\n", FIXTURE_HOLD.as_secs()),
        );

        // The child outlives any plausible test run, so the probe can only end
        // through its own deadline; a probe that waited for the child would
        // report `Output` instead.
        let outcome = probe_hermes_version(&hermes, Duration::from_millis(50), &[]);

        assert!(matches!(outcome, ProbeOutcome::TimedOut), "{outcome:?}");
    }

    #[test]
    fn hermes_version_probe_honors_its_timeout_argument() {
        const STEPS: u32 = 4;
        let dir = temp_agents_dir();
        let hermes = dir.join("hermes");
        write_test_executable(
            &hermes,
            &format!("#!/bin/sh\nsleep {}\n", FIXTURE_HOLD.as_secs()),
        );
        // The fake clock advances by a quarter of the timeout on every read. The
        // first read starts the deadline; the child never exits, so the probe
        // must stop on exactly the read that reaches `start + timeout`.
        let timeout = Duration::from_secs(8);
        let start = Instant::now();
        let reads = Cell::new(0_u32);
        let clock = || {
            let read = reads.get();
            reads.set(read + 1);
            start + (timeout / STEPS) * read
        };

        let outcome = probe_hermes_version_with_clock(&hermes, timeout, &[], clock);

        assert!(matches!(outcome, ProbeOutcome::TimedOut), "{outcome:?}");
        assert_eq!(
            reads.get(),
            STEPS + 1,
            "the probe must expire when the clock reaches start + timeout"
        );
    }

    #[test]
    fn hermes_version_probe_timeout_kills_descendants() {
        let dir = temp_agents_dir();
        let hermes = dir.join("hermes");
        let pid_file = dir.join("descendant.pid");
        // Both sleepers are started before the descendant records its pid, so
        // the whole tree exists when the test observes it.
        write_test_executable(
            &hermes,
            &format!(
                "#!/bin/sh\n\
                 sleep {hold} &\n\
                 sh -c 'echo $$ > {pid_file}; exec sleep {hold}' &\n\
                 wait\n",
                pid_file = pid_file.display(),
                hold = FIXTURE_HOLD.as_secs(),
            ),
        );
        let inspector = HostInspector::new();
        let own_group = process_group_of(inspector, std::process::id());
        let tree = RefCell::new(Vec::new());
        let _leak_guard = TreeGuard {
            inspector,
            tree: &tree,
        };

        // The deadline is driven by a clock that expires only once the test has
        // observed the live process tree: the first read starts the deadline and
        // the next one waits for the descendant's pid, records the tree with its
        // start identities, checks the kernel's process groups, and then reports
        // the deadline as reached. No attempt can time out before the tree exists.
        let timeout = Duration::from_secs(1);
        let start = Instant::now();
        let reads = Cell::new(0_u32);
        let clock = || {
            let read = reads.get();
            reads.set(read + 1);
            if read == 0 {
                return start;
            }
            if tree.borrow().is_empty() {
                observe_probe_tree(inspector, &pid_file, own_group, &tree);
            }
            start + timeout
        };
        let outcome = probe_hermes_version_with_clock(&hermes, timeout, &[], clock);
        assert!(matches!(outcome, ProbeOutcome::TimedOut), "{outcome:?}");

        poll_until("the probe's process tree to be terminated", || {
            let running = tree.borrow().iter().any(|identity| {
                inspector
                    .is_running(*identity)
                    .expect("inspect the probe's process tree")
            });
            (!running).then_some(())
        });
    }

    /// Waits for the fixture's descendant, records the probe child and every
    /// process below it, then asserts the probe isolated them in a group of
    /// their own.
    ///
    /// The tree is recorded before the assertions so a failed assertion still
    /// leaves it to the caller's [`TreeGuard`].
    fn observe_probe_tree(
        inspector: HostInspector,
        pid_file: &Path,
        own_group: Pid,
        tree: &RefCell<Vec<ProcessIdentity>>,
    ) {
        let descendant = poll_until("the descendant to record its pid", || {
            std::fs::read_to_string(pid_file)
                .ok()
                .and_then(|text| text.trim().parse::<Pid>().ok())
        });
        let fact = inspector
            .process(descendant)
            .expect("inspect the descendant")
            .expect("the descendant is live while the probe waits");
        let leader = fact.ppid;
        let mut identities = vec![inspector
            .identity(leader)
            .expect("inspect the probe child")
            .expect("the probe child is live")];
        identities.extend(
            inspector
                .descendants(leader)
                .expect("inspect the probe child's descendants")
                .iter()
                .map(ProcessFact::identity),
        );
        tree.replace(identities);
        assert_probe_isolation(fact.pgid, leader, own_group);
    }

    /// Asserts the descendant runs in a group led by the probe child that is not
    /// the test's own, so terminating the group cannot reach the test run.
    fn assert_probe_isolation(group: Pid, leader: Pid, own_group: Pid) {
        assert_ne!(
            group, own_group,
            "the probe must not leave its child in the caller's process group"
        );
        assert_eq!(
            group, leader,
            "the probe must run its child as the leader of a new process group"
        );
    }

    fn process_group_of(inspector: HostInspector, pid: Pid) -> Pid {
        inspector
            .process(pid)
            .expect("inspect process")
            .expect("process is live")
            .pgid
    }

    #[test]
    fn probe_isolation_accepts_a_group_led_by_the_probe_child() {
        assert_probe_isolation(200, 200, 100);
    }

    #[test]
    #[should_panic(expected = "must not leave its child in the caller's process group")]
    fn probe_isolation_rejects_the_callers_group() {
        assert_probe_isolation(100, 200, 100);
    }

    #[test]
    #[should_panic(expected = "leader of a new process group")]
    fn probe_isolation_rejects_a_group_the_probe_child_does_not_lead() {
        assert_probe_isolation(300, 200, 100);
    }

    /// Kills the recorded probe tree on drop, so a failed assertion or wait does
    /// not leak the fixture's long-running sleepers.
    ///
    /// Each process is signalled only while its exact identity (pid and start
    /// identity) is still running, so a recycled pid is never signalled; the
    /// check-then-kill gap is the only residual race. Signalling by pid never
    /// reaches the test's own process group.
    struct TreeGuard<'tree> {
        inspector: HostInspector,
        tree: &'tree RefCell<Vec<ProcessIdentity>>,
    }

    impl Drop for TreeGuard<'_> {
        fn drop(&mut self) {
            for identity in self.tree.borrow().iter() {
                if !matches!(self.inspector.is_running(*identity), Ok(true)) {
                    continue;
                }
                let Ok(pid) = i32::try_from(identity.pid) else {
                    continue;
                };
                // SAFETY: the pid was observed as a member of the fixture's tree
                // and its start identity was just re-verified.
                #[expect(unsafe_code, reason = "cleanup of leaked test processes")]
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }

    #[test]
    fn hermes_version_probe_output_is_bounded() {
        let dir = temp_agents_dir();
        let hermes = dir.join("hermes");
        let marker = dir.join("writer-status.txt");
        write_test_executable(
            &hermes,
            &format!(
                "#!/bin/sh\n\
                 head -c 1048576 /dev/zero\n\
                 writer_status=$?\n\
                 printf '%s' \"$writer_status\" > {marker}\n\
                 exit \"$writer_status\"\n",
                marker = marker.display(),
            ),
        );

        // The deadline is far beyond any plausible scheduling delay, so a
        // `Failed` outcome can only come from the output limit terminating the
        // writer, never from the probe's own timeout.
        let outcome = probe_hermes_version(&hermes, HANG_GUARD, &[]);

        assert!(matches!(outcome, ProbeOutcome::Failed), "{outcome:?}");
        assert_eq!(
            std::fs::read_to_string(marker).expect("writer status marker"),
            (128 + libc::SIGXFSZ).to_string(),
            "the writer must be terminated by SIGXFSZ at the output limit"
        );
    }

    #[test]
    fn hermes_profile_executable_override_must_be_executable() {
        let dir = temp_agents_dir();
        let non_executable = dir.join("hermes-wrapper");
        std::fs::write(&non_executable, "#!/bin/sh\n").expect("write wrapper");
        std::fs::write(
            dir.join("hermes-work.toml"),
            format!(
                "base = \"hermes\"\nprogram = \"{}\"\nargs = [\"chat\"]\n",
                non_executable.display()
            ),
        )
        .expect("write profile");

        let caps = host_capabilities("0.0.0", &ProfileRegistry::new(Some(dir.clone())));
        let runtime = caps
            .runtimes
            .iter()
            .find(|runtime| runtime.agent == "hermes-work")
            .expect("Hermes profile runtime");
        assert!(!runtime.available);
        assert_eq!(runtime.agent_base, Some(RuntimeRef::hermes()));
        assert_eq!(runtime.supported, None);
    }

    fn write_test_executable(path: &Path, body: &str) {
        std::fs::write(path, body).expect("write executable");
        let mut permissions = std::fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions).expect("set executable mode");
    }
    #[test]
    fn semver_probe_reports_and_validates_the_declared_range() {
        let dir = temp_agents_dir();
        let tool = dir.join("tool");
        let binary = tool.display().to_string();
        let definition = probed_definition(SEMVER_PROBE);

        write_test_executable(&tool, "#!/bin/sh\necho 1.0.2\n");
        let supported =
            probe_versioned_runtime("tool", RuntimeRef::from_wire("tool"), &binary, &definition);
        assert!(supported.available);
        assert_eq!(supported.version.as_deref(), Some("1.0.2"));
        assert_eq!(supported.supported, Some(true));
        let validated = validate_definition_launch(&definition, &binary)
            .expect("a release inside the range launches")
            .expect("the probed executable is pinned");
        assert_eq!(validated.as_path(), tool.canonicalize().expect("canonical"));

        for (output, version) in [
            ("1.1.0", Some("1.1.0")),
            ("0.99.0", Some("0.99.0")),
            ("pi 1.0.2", None),
            ("1.0.2-beta", None),
            ("", None),
        ] {
            write_test_executable(&tool, &format!("#!/bin/sh\necho '{output}'\n"));
            let runtime = probe_versioned_runtime(
                "tool",
                RuntimeRef::from_wire("tool"),
                &binary,
                &definition,
            );
            assert!(runtime.available, "{output:?}");
            assert_eq!(runtime.version.as_deref(), version, "{output:?}");
            assert_eq!(runtime.supported, Some(false), "{output:?}");
            let error = validate_definition_launch(&definition, &binary)
                .expect_err("a release outside the range is refused");
            assert_eq!(error.code, "agent_runtime_unsupported");
            assert!(!error.msg.contains(output) || output.is_empty());
        }

        write_test_executable(&tool, "#!/bin/sh\necho 1.0.2\nexit 3\n");
        let failing =
            probe_versioned_runtime("tool", RuntimeRef::from_wire("tool"), &binary, &definition);
        assert_eq!(
            failing.supported,
            Some(false),
            "a failing probe is unsupported"
        );

        let missing = probe_versioned_runtime(
            "tool",
            RuntimeRef::from_wire("tool"),
            &dir.join("missing").display().to_string(),
            &definition,
        );
        assert!(!missing.available);
        assert_eq!(missing.supported, None);
    }

    #[test]
    fn semver_probe_passes_the_declared_arguments_and_only_the_search_path() {
        let dir = temp_agents_dir();
        let tool = dir.join("tool");
        let marker = dir.join("isolation.txt");
        write_test_executable(
            &tool,
            &format!(
                "#!/bin/sh\n\
                 [ \"$#\" = 2 ] || exit 20\n\
                 [ \"$1\" = -V ] || exit 21\n\
                 [ \"$2\" = --plain ] || exit 22\n\
                 [ \"${{POHUNEK_PROBE_SENTINEL+x}}\" != x ] || exit 23\n\
                 [ \"${{PI_CODING_AGENT_DIR+x}}\" != x ] || exit 24\n\
                 [ \"${{HERMES_HOME+x}}\" != x ] || exit 25\n\
                 [ \"${{PYTHONNOUSERSITE+x}}\" != x ] || exit 26\n\
                 [ \"$PATH\" = '{path}' ] || exit 27\n\
                 [ \"${{HOME##*/}}\" = home ] || exit 28\n\
                 [ \"${{XDG_CONFIG_HOME##*/}}\" = xdg-config ] || exit 29\n\
                 [ \"${{TMPDIR##*/}}\" = tmp ] || exit 30\n\
                 [ -d \"$HOME\" ] || exit 31\n\
                 printf 'isolated' > {marker}\n\
                 echo 1.0.2\n",
                marker = marker.display(),
                path = std::env::var("PATH").expect("the test process has a PATH"),
            ),
        );
        let definition = probed_definition(
            r#"{ parser = "semver-v1", args = ["-V", "--plain"], min = "1.0.0", below = "1.1.0" }"#,
        );
        let policy = definition.version_probe_policy().expect("policy");
        let seeded_env = [
            ("POHUNEK_PROBE_SENTINEL", "ambient"),
            ("PI_CODING_AGENT_DIR", "ambient"),
            ("HOME", "ambient-home-sentinel"),
        ];
        let outcome = spawn_and_collect_probe(
            &tool,
            ProbeKind::Semver(policy),
            HANG_GUARD,
            &seeded_env,
            &Instant::now,
        )
        .expect("the probe runs");
        assert!(
            matches!(&outcome, ProbeOutcome::Output(output) if output.trim() == "1.0.2"),
            "{outcome:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&marker).expect("marker"),
            "isolated"
        );
    }

    #[test]
    fn semver_parser_without_its_policy_or_with_foreign_fields_is_not_a_definition() {
        let document = |probe: &str| {
            format!(
                "schema = 1\nid = \"acme.runtime.tool\"\nversion = \"1.0.0\"\nruntime_api = 1\n\n\
                 [runtime]\nid = \"tool\"\nname = \"Tool\"\nprogram = \"tool\"\nargs = []\n\
                 detect_manifest = \"any\"\nversion_probe = {probe}\n\n\
                 [input]\nbracketed_paste = false\nsubmit_delay_ms = 0\ntext_policy = \"unrestricted\"\n\n\
                 [resume]\nsupported = false\n\n[fork]\nsupported = false\n\n\
                 [native_reference]\nstrategy = \"none\"\n"
            )
        };
        let parse = |probe: &str| {
            RuntimeDefinition::from_toml(
                &document(probe),
                |package| DefinitionOrigin::Package {
                    package,
                    digest: protocol::PackageDigest::parse(&format!("sha256:{}", "3".repeat(64)))
                        .expect("digest"),
                },
                |_| Ok(Arc::new(crate::detect::generic_shell_manifest().clone())),
            )
        };
        for invalid in [
            r#"{ parser = "semver-v1" }"#,
            r#"{ parser = "semver-v1", args = ["--version"], min = "1.0.0" }"#,
            r#"{ parser = "hermes-v1", args = ["--version"], min = "1.0.0", below = "1.1.0" }"#,
            r#"{ parser = "semver-v1", args = ["--version"], min = "1.1.0", below = "1.0.0" }"#,
        ] {
            assert!(
                matches!(
                    parse(invalid),
                    Err(crate::agent::host::DefinitionError::Field { .. })
                ),
                "{invalid}"
            );
        }
        parse(SEMVER_PROBE).expect("the data-driven probe parses");
        parse(r#"{ parser = "hermes-v1" }"#).expect("the compiled probe parses");
    }
}
