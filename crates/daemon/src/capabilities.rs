//! Live host-capability snapshot builder for `host.inspect`.
//!
//! Builds a [`HostCapabilities`] describing what this host can do right now:
//! which protocol version the daemon speaks, which agent kinds it supports,
//! which agent runtimes are actually installed (probed against `PATH`), and
//! whether git-backed worktree sessions are available. Presence and path are
//! resolved fresh on every request; the version probes behind them are shared
//! through a [`ProbeCache`] keyed by the probed file's identity, so a program
//! replaced on disk is probed again at once and an unchanged one at most once
//! per configured lifetime.

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::{
    AgentRuntime, HostCapabilities, ProtocolError, RuntimeId, RuntimeRef, PROTOCOL_VERSION,
};
use tracing::debug;

use crate::agent::host::{
    LaunchProgram, RuntimeDefinition, RuntimeRegistry, Spawn, VersionProbePolicy, VersionRefusal,
    RESERVED_RUNTIME_IDS, SEMVER_LINE_PARSER_ID, SEMVER_PARSER_ID, SEMVER_REVISION_LINE_PARSER_ID,
};
use crate::agent::{which_executable, ProfileRegistry, ValidatedLaunchProgram};
use crate::integration::Home;
use crate::runtime::environment::effective_variable;
use pohunek_worker_protocol::BaseEnv;

mod probe_cache;

pub(crate) use probe_cache::{ProbeCache, ProbeRequest};

/// Maximum wall time for a local version probe, including interpreter startup.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Maximum stdout the version probe may write or retain.
const VERSION_OUTPUT_LIMIT: usize = 4 * 1024;
/// Poll cadence balances bounded shutdown with negligible idle CPU use.
const VERSION_PROBE_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// A deterministic locale keeps provider version output stable.
const PROBE_LOCALE: &str = "C";

/// Version-probe parsers the daemon compiles in, selected by the parser id a
/// runtime definition names under `version_probe.parser`.
///
/// The compiled parsers (`semver-v1`, `semver-line-v1` and
/// `semver-revision-line-v1`) are data-driven: the declared arguments print one
/// `MAJOR.MINOR.PATCH` release (with a `-R<n>[.<m>]` revision for the last),
/// bare or inside the declared line template, that must lie in the declared
/// range, so a package update moves the range without a daemon release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VersionProbe<'a>(&'a VersionProbePolicy);

impl<'a> VersionProbe<'a> {
    /// Resolves a definition's parser id, or `None` for an id this daemon does
    /// not compile in or a data-driven parser without its policy.
    fn for_definition(definition: &'a RuntimeDefinition) -> Option<Self> {
        match definition.version_probe_parser()?.as_str() {
            SEMVER_PARSER_ID | SEMVER_LINE_PARSER_ID | SEMVER_REVISION_LINE_PARSER_ID => {
                definition.version_probe_policy().map(Self)
            }
            _ => None,
        }
    }

    /// The version the declared grammar reads from raw probe `output`.
    fn read_version(self, output: &str) -> Option<String> {
        self.0
            .read_output(output)
            .map(|version| version.to_string())
    }

    /// Whether this daemon accepts `version` for launch.
    fn accepts(self, version: &str) -> bool {
        let parsed = if self.0.reads_revisions() {
            crate::agent::host::ProbeVersion::parse_revision(version)
        } else {
            crate::agent::host::ProbeVersion::parse(version)
        };
        parsed.is_some_and(|version| self.0.supports(version))
    }
}

/// Position of a runtime id in the reported inventory: the reserved ids (the
/// shell and the official aliases) keep their documented order, every other
/// runtime follows in id order.
fn inventory_rank(runtime_id: &RuntimeId) -> usize {
    RESERVED_RUNTIME_IDS
        .iter()
        .position(|reserved| *reserved == runtime_id.as_str())
        .unwrap_or(RESERVED_RUNTIME_IDS.len())
}

/// Build the live capability snapshot for this host.
///
/// `supported_agents` is every runtime the registry serves (the built-in shell
/// and every installed, enabled and selected package runtime) plus every
/// resolvable host agent profile (Part C). `runtimes` reports, per agent, whether
/// its backing program is present on `PATH`: a runtime launched through the
/// host's login shell is always available (no path), and every other runtime
/// probes its (possibly-overridden) program. A runtime whose definition names a
/// version-probe parser additionally reports its version and whether the daemon
/// accepts it. Probing uses the same executable check as the launch path
/// ([`which_executable`]) so "available" agrees with what a launch would accept.
/// A version probe runs under the `PATH` the launch would see: `base_env` (the
/// environment the daemon forwards to agents) overridden by the profile's own
/// environment. The probes of all entries are collected first and run once per
/// distinct program, in parallel, through `cache`; blocking work, so an async
/// caller runs this on a blocking thread.
/// `git_available` reflects a `git` probe and currently also gates worktree support.
#[must_use]
pub(crate) fn host_capabilities(
    daemon_version: &str,
    profiles: &ProfileRegistry,
    base_env: &BaseEnv,
    cache: &ProbeCache,
) -> HostCapabilities {
    host_capabilities_for(
        daemon_version,
        profiles,
        &profiles.runtimes().registry(),
        base_env,
        cache,
    )
}

/// The capability snapshot without any runtime inventory: the feature flags,
/// the daemon version and `git`, with empty `supported_agents` and `runtimes`.
///
/// This is what `host.inspect` answers for a client that only needs the flags;
/// it resolves and probes no program besides `git`.
#[must_use]
pub(crate) fn host_capability_flags(daemon_version: &str) -> HostCapabilities {
    capability_snapshot(daemon_version, Vec::new(), Vec::new())
}

/// [`host_capabilities`] through a probe cache of its own, so a call never
/// sees a result an earlier call stored.
#[cfg(test)]
pub(crate) fn host_capabilities_uncached(
    daemon_version: &str,
    profiles: &ProfileRegistry,
    base_env: &BaseEnv,
) -> HostCapabilities {
    host_capabilities(
        daemon_version,
        profiles,
        base_env,
        &ProbeCache::new(pohunek_service_config::DEFAULT_INVENTORY_PROBE),
    )
}

/// [`host_capabilities_for`] through a probe cache of its own.
#[cfg(test)]
fn host_capabilities_for_uncached(
    daemon_version: &str,
    profiles: &ProfileRegistry,
    registry: &RuntimeRegistry,
    base_env: &BaseEnv,
) -> HostCapabilities {
    host_capabilities_for(
        daemon_version,
        profiles,
        registry,
        base_env,
        &ProbeCache::new(pohunek_service_config::DEFAULT_INVENTORY_PROBE),
    )
}

/// A runtime entry whose version probe is still to be read from the probe
/// results: the entry sits at `index` of the inventory.
struct PendingProbe {
    index: usize,
    request: ProbeRequest,
    policy: VersionProbePolicy,
}

/// [`host_capabilities`] against an explicit runtime registry.
fn host_capabilities_for(
    daemon_version: &str,
    profiles: &ProfileRegistry,
    registry: &RuntimeRegistry,
    base_env: &BaseEnv,
    cache: &ProbeCache,
) -> HostCapabilities {
    let mut definitions: Vec<&Arc<RuntimeDefinition>> = registry.definitions().collect();
    // The sort is stable, so runtimes outside the reserved set stay in id order.
    definitions.sort_by_key(|definition| inventory_rank(definition.runtime_id()));

    let mut supported_agents = Vec::with_capacity(definitions.len());
    let mut runtimes = Vec::with_capacity(definitions.len());
    let mut pending = Vec::new();
    for definition in definitions {
        let agent = definition.runtime_id().as_str();
        let agent_base = RuntimeRef::from(definition.runtime_id().clone());
        let planned = match definition.program() {
            // A host-shell runtime needs no named binary on PATH.
            LaunchProgram::HostShell(_) => (
                AgentRuntime {
                    agent: agent.to_owned(),
                    agent_base: Some(agent_base),
                    available: true,
                    path: None,
                    version: None,
                    supported: None,
                    config_home_id: None,
                },
                None,
            ),
            LaunchProgram::Fixed(program) => {
                probe_definition(agent, agent_base, definition, program, base_env, &[])
            }
        };
        let entry = place_runtime(&mut runtimes, &mut pending, planned);
        entry.config_home_id = config_home_id(profiles, definition, base_env, &[]);
        supported_agents.push(agent.to_owned());
    }

    // Host profiles: each resolvable profile is a launchable agent name; probe its
    // resolved program exactly as its base runtime, so availability is consistent.
    for agent in profiles.enumerate() {
        // A pinned profile is described by the definition it launches from,
        // and is unavailable once that package cannot launch a fresh session.
        let launchable = profiles.runtimes().verify_launchable(&agent.definition);
        let agent_base = RuntimeRef::from(agent.base.clone());
        let planned = match (&launchable, agent.profile.as_ref()) {
            (Err(_), _) => (unavailable_runtime(&agent.name, agent_base), None),
            (Ok(()), Some(profile)) => probe_definition(
                &agent.name,
                agent_base,
                &agent.definition,
                &profile.program,
                base_env,
                &profile.env,
            ),
            (Ok(()), None) => probe_definition(
                &agent.name,
                agent_base,
                &agent.definition,
                agent.definition.program().as_str(),
                base_env,
                &[],
            ),
        };
        let entry = place_runtime(&mut runtimes, &mut pending, planned);
        if launchable.is_ok() {
            if let Some(profile) = agent.profile.as_ref() {
                entry.config_home_id =
                    config_home_id(profiles, &agent.definition, base_env, &profile.env);
            }
        }
        supported_agents.push(agent.name);
    }
    for profile in profiles.unavailable_pinned() {
        runtimes.push(unavailable_runtime(
            &profile.name,
            RuntimeRef::from(profile.base),
        ));
        supported_agents.push(profile.name);
    }

    let requests: Vec<&ProbeRequest> = pending.iter().map(|probe| &probe.request).collect();
    let outputs = cache.fetch(&requests, Instant::now());
    apply_probe_outputs(&mut runtimes, &pending, outputs);

    capability_snapshot(daemon_version, supported_agents, runtimes)
}

/// Reads each pending probe's raw `outputs` with its runtime's own grammar and
/// range, and records the version and support on the inventory entry.
fn apply_probe_outputs(
    runtimes: &mut [AgentRuntime],
    pending: &[PendingProbe],
    outputs: Vec<Option<String>>,
) {
    for (probe, output) in pending.iter().zip(outputs) {
        let version = output
            .as_deref()
            .and_then(|output| VersionProbe(&probe.policy).read_version(output));
        let supported = version
            .as_deref()
            .is_some_and(|version| VersionProbe(&probe.policy).accepts(version));
        let entry = &mut runtimes[probe.index];
        entry.version = version;
        entry.supported = Some(supported);
    }
}

/// Appends `planned` to the inventory, queues its probe, and returns the entry.
fn place_runtime<'a>(
    runtimes: &'a mut Vec<AgentRuntime>,
    pending: &mut Vec<PendingProbe>,
    (runtime, probe): PlannedRuntime,
) -> &'a mut AgentRuntime {
    if let Some((request, policy)) = probe {
        pending.push(PendingProbe {
            index: runtimes.len(),
            request,
            policy,
        });
    }
    runtimes.push(runtime);
    runtimes
        .last_mut()
        .expect("the entry was pushed just above")
}

/// The feature flags every snapshot carries around the inventory.
fn capability_snapshot(
    daemon_version: &str,
    supported_agents: Vec<String>,
    runtimes: Vec<AgentRuntime>,
) -> HostCapabilities {
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
        session_wait_turn_supported: true,
        session_new_cwd_supported: true,
        session_new_input_ack_supported: true,
        session_idle_stop_supported: true,
        session_new_branch_source_supported: true,
    }
}

/// The opaque identifier of the config home a launch of `definition` with
/// the profile environment `profile_env` gives its agent.
///
/// The home is resolved by the same rule the integration lifecycle and the
/// transcript observer use ([`Home::resolve`]). `None` when the runtime
/// declares no config home, the home cannot be resolved, or the host cannot
/// key the identifier: an inventory never fails on it.
fn config_home_id(
    profiles: &ProfileRegistry,
    definition: &RuntimeDefinition,
    base_env: &BaseEnv,
    profile_env: &[(String, String)],
) -> Option<String> {
    let declared = definition.config_home()?;
    let home = match Home::resolve(declared, base_env, profile_env) {
        Ok(home) => home,
        Err(error) => {
            debug!(
                runtime = definition.runtime_id().as_str(),
                code = %error.code,
                "config home does not resolve; reporting no config_home_id"
            );
            return None;
        }
    };
    profiles
        .revision_keys()
        .config_home_id(&home.identity())
        .ok()
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
        config_home_id: None,
    }
}

/// A runtime entry plus the version probe still to run for it, when it has one.
type PlannedRuntime = (AgentRuntime, Option<(ProbeRequest, VersionProbePolicy)>);

/// Probe `program` for `definition`, honouring its version-probe parser.
fn probe_definition(
    agent: &str,
    agent_base: RuntimeRef,
    definition: &RuntimeDefinition,
    program: &str,
    base_env: &BaseEnv,
    profile_env: &[(String, String)],
) -> PlannedRuntime {
    match definition.version_probe_parser() {
        Some(_) => {
            let launch_path = effective_variable(base_env, profile_env, SEARCH_PATH_VAR);
            plan_versioned_runtime(
                agent,
                agent_base,
                program,
                definition,
                launch_path.as_deref(),
            )
        }
        None => (probe_runtime(agent, agent_base, program), None),
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
            config_home_id: None,
        },
        None => AgentRuntime {
            agent: agent.to_owned(),
            agent_base: Some(agent_base),
            available: false,
            path: None,
            version: None,
            supported: None,
            config_home_id: None,
        },
    }
}

/// Resolve an executable for the compiled parser `definition` names and plan
/// its version probe, without running it.
///
/// A present runtime always reports `supported` once the probe result is
/// read: `Some(true)` only for an accepted version, `Some(false)` for any
/// other version, unparseable output or a parser id this daemon does not
/// compile in (which plans no probe, so the entry starts at `Some(false)`).
fn plan_versioned_runtime(
    agent: &str,
    agent_base: RuntimeRef,
    binary: &str,
    definition: &RuntimeDefinition,
    launch_path: Option<&OsStr>,
) -> PlannedRuntime {
    let Some(program) = ValidatedLaunchProgram::resolve(binary) else {
        return (
            AgentRuntime {
                agent: agent.to_owned(),
                agent_base: Some(agent_base),
                available: false,
                path: None,
                version: None,
                supported: None,
                config_home_id: None,
            },
            None,
        );
    };

    let probe = VersionProbe::for_definition(definition).map(|probe| {
        (
            ProbeRequest::new(
                program.as_path(),
                probe.0.args(),
                launch_path,
                definition.launch_env(),
            ),
            probe.0.clone(),
        )
    });
    (
        AgentRuntime {
            agent: agent.to_owned(),
            agent_base: Some(agent_base),
            available: true,
            path: Some(program.as_path().display().to_string()),
            version: None,
            supported: Some(false),
            config_home_id: None,
        },
        probe,
    )
}

/// Plans and runs one runtime's version probe through a probe cache of its
/// own, returning the finished inventory entry.
#[cfg(test)]
fn probe_versioned_runtime(
    agent: &str,
    agent_base: RuntimeRef,
    binary: &str,
    definition: &RuntimeDefinition,
    launch_path: Option<&OsStr>,
) -> AgentRuntime {
    probed_entry(plan_versioned_runtime(
        agent,
        agent_base,
        binary,
        definition,
        launch_path,
    ))
}

/// Runs the probe of `planned`, if any, through a probe cache of its own.
#[cfg(test)]
fn probed_entry((runtime, probe): PlannedRuntime) -> AgentRuntime {
    let mut runtimes = vec![runtime];
    let pending: Vec<PendingProbe> = probe
        .into_iter()
        .map(|(request, policy)| PendingProbe {
            index: 0,
            request,
            policy,
        })
        .collect();
    let requests: Vec<&ProbeRequest> = pending.iter().map(|probe| &probe.request).collect();
    let cache = ProbeCache::new(pohunek_service_config::DEFAULT_INVENTORY_PROBE);
    let outputs = cache.fetch(&requests, Instant::now());
    apply_probe_outputs(&mut runtimes, &pending, outputs);
    runtimes.remove(0)
}

/// [`crate::agent::host::validate_launch_runtime`] for an already resolved definition.
///
/// # Errors
///
/// Returns `agent_binary_missing`, naming only the program's file name, when
/// the program does not resolve to an executable, and
/// `agent_runtime_unsupported` when the probe rejects an executable that
/// exists or the definition names a parser this daemon does not provide.
///
/// `launch_path` is the `PATH` the launched agent will see; the probe runs
/// under it.
pub(crate) fn validate_definition_launch(
    definition: &RuntimeDefinition,
    spawn: Spawn<'_>,
    launch_path: Option<&OsStr>,
) -> Result<Option<ValidatedLaunchProgram>, ProtocolError> {
    validate_definition_launch_with_timeout(definition, spawn, launch_path, VERSION_PROBE_TIMEOUT)
}

/// Validate a launch definition using the caller's remaining version-probe budget.
pub(crate) fn validate_definition_launch_with_timeout(
    definition: &RuntimeDefinition,
    spawn: Spawn<'_>,
    launch_path: Option<&OsStr>,
    timeout: Duration,
) -> Result<Option<ValidatedLaunchProgram>, ProtocolError> {
    validate_definition_launch_with_timeout_detailed(definition, spawn, launch_path, timeout)
        .map_err(|failure| match failure {
            LaunchValidationFailure::Missing => {
                ProtocolError::agent_binary_missing(reported_program(definition, spawn))
            }
            LaunchValidationFailure::Refused(refusal) => unsupported_version_error(refusal),
            LaunchValidationFailure::Unsupported | LaunchValidationFailure::ProbeTimedOut => {
                ProtocolError::agent_runtime_unsupported()
            }
        })
}

/// The `agent_runtime_unsupported` refusal for probe output that ran to
/// completion: it names the parsed release and the supported range, or that
/// the output did not parse, and never the output itself.
pub(crate) fn unsupported_version_error(refusal: VersionRefusal) -> ProtocolError {
    match refusal {
        VersionRefusal::Unparseable => ProtocolError::agent_runtime_version_unparseable(),
        VersionRefusal::OutOfRange {
            version,
            min,
            below,
        } => ProtocolError::agent_runtime_version_out_of_range(&version, &min, &below),
    }
}

/// The program name a missing-executable refusal reports: the file name of
/// the spawned program, so a configured absolute path never reaches the
/// error, else the runtime id.
fn reported_program<'a>(definition: &'a RuntimeDefinition, spawn: Spawn<'a>) -> &'a str {
    match spawn {
        Spawn::Program(binary) => Path::new(binary).file_name().and_then(OsStr::to_str),
        Spawn::HostShell => None,
    }
    .unwrap_or_else(|| definition.runtime_id().as_str())
}

/// Why a launch-time validation failed, retaining a shortened-budget timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaunchValidationFailure {
    /// The program does not resolve to an executable; no probe ran.
    Missing,
    /// The probe exited successfully, and its output names no supported
    /// release.
    Refused(VersionRefusal),
    /// The probe failed (a non-zero exit, its full deadline, or a setup
    /// failure), or the definition names a parser this daemon does not
    /// provide.
    Unsupported,
    /// The probe outran a budget shorter than [`VERSION_PROBE_TIMEOUT`].
    ProbeTimedOut,
}

/// Validate a launch definition and preserve whether its version probe timed out.
///
/// The spawned program is resolved first for every launch, so a missing
/// executable is told apart from one the probe rejects. A bare host-shell
/// launch ([`Spawn::HostShell`]) is the exception: it spawns the configured
/// shell command, which its launch resolves. A runtime with a version probe
/// is never a host shell, so such a launch is refused rather than unprobed.
pub(crate) fn validate_definition_launch_with_timeout_detailed(
    definition: &RuntimeDefinition,
    spawn: Spawn<'_>,
    launch_path: Option<&OsStr>,
    timeout: Duration,
) -> Result<Option<ValidatedLaunchProgram>, LaunchValidationFailure> {
    let probed = definition.version_probe_parser().is_some();
    let binary = match spawn {
        Spawn::HostShell if probed => return Err(LaunchValidationFailure::Unsupported),
        Spawn::HostShell => return Ok(None),
        Spawn::Program(binary) => binary,
    };
    if !probed {
        // The same resolution the launch applies to a program it spawns by
        // name: an executable on the daemon's `PATH` with a UTF-8 path.
        return if which_executable(binary).is_some_and(|path| path.to_str().is_some()) {
            Ok(None)
        } else {
            Err(LaunchValidationFailure::Missing)
        };
    }
    let shortened_budget = timeout < VERSION_PROBE_TIMEOUT;
    let timeout = timeout.min(VERSION_PROBE_TIMEOUT);

    let program =
        ValidatedLaunchProgram::resolve(binary).ok_or(LaunchValidationFailure::Missing)?;
    let Some(probe) = VersionProbe::for_definition(definition) else {
        return Err(LaunchValidationFailure::Unsupported);
    };
    let policy = probe.0;
    let outcome = spawn_and_collect_probe(
        program.as_path(),
        policy,
        launch_path,
        definition.launch_env(),
        timeout,
        &[],
        &Instant::now,
    )
    .unwrap_or(ProbeOutcome::Unavailable);
    match outcome {
        ProbeOutcome::Output(output) => policy
            .check_output(&output)
            .map(|_version| Some(program))
            .map_err(LaunchValidationFailure::Refused),
        ProbeOutcome::TimedOut if shortened_budget => Err(LaunchValidationFailure::ProbeTimedOut),
        ProbeOutcome::TimedOut | ProbeOutcome::Failed | ProbeOutcome::Unavailable => {
            Err(LaunchValidationFailure::Unsupported)
        }
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
    /// The child exited unsuccessfully, including termination by a signal, or
    /// wrote more than the output limit.
    Failed,
    /// The deadline expired and the child's process group was terminated.
    TimedOut,
    /// The probe could not be set up, polled or read.
    Unavailable,
}

/// The environment variable that holds the executable search path.
const SEARCH_PATH_VAR: &str = "PATH";

/// Runs the probe `request` describes and returns its stdout, or `None` for
/// every outcome that is not a successful exit.
fn run_request(request: &ProbeRequest) -> Option<String> {
    match collect_probe(
        request.program(),
        request.args(),
        request.launch_path(),
        request.launch_env(),
        VERSION_PROBE_TIMEOUT,
        &[],
        &Instant::now,
    )? {
        ProbeOutcome::Output(output) => Some(output),
        ProbeOutcome::Failed | ProbeOutcome::TimedOut | ProbeOutcome::Unavailable => None,
    }
}

/// Runs `path` with the arguments `policy` declares under a runtime-neutral
/// environment plus `launch_path`, the `PATH` the launch sees, and collects
/// its bounded output.
fn spawn_and_collect_probe(
    path: &std::path::Path,
    policy: &VersionProbePolicy,
    launch_path: Option<&OsStr>,
    launch_env: &[(String, String)],
    timeout: Duration,
    seeded_env: &[(&str, &str)],
    now: &impl Fn() -> Instant,
) -> Option<ProbeOutcome> {
    collect_probe(
        path,
        policy.args(),
        launch_path,
        launch_env,
        timeout,
        seeded_env,
        now,
    )
}

/// Spawns `path` with `args` and collects at most [`VERSION_OUTPUT_LIMIT`]
/// bytes of its stdout.
///
/// The child is spawned without `pre_exec` so std can use `posix_spawn`, whose
/// cost does not grow with the daemon's address space. Its stdout is a pipe the
/// daemon drains while it waits; output past the limit ends the probe's process
/// group and counts as a failed probe.
fn collect_probe(
    path: &std::path::Path,
    args: &[String],
    launch_path: Option<&OsStr>,
    launch_env: &[(String, String)],
    timeout: Duration,
    seeded_env: &[(&str, &str)],
    now: &impl Fn() -> Instant,
) -> Option<ProbeOutcome> {
    let sandbox = VersionProbeSandbox::create()?;
    let mut command = Command::new(path);
    command.envs(seeded_env.iter().copied());
    // The runtime's `[launch_env]` pairs go in right after the clear: they
    // never name a variable the sandbox below sets, and a program that
    // self-updates unless one is set needs it during the probe too.
    command.env_clear().envs(launch_env.iter().cloned());
    command
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
    // The probed program may be an interpreter script (`#!/usr/bin/env
    // node`); the effective launch `PATH` (daemon base plus profile
    // overrides) finds the interpreter the launch will use. Every other
    // variable stays isolated.
    command.args(args);
    if let Some(search_path) = launch_path {
        command.env(SEARCH_PATH_VAR, search_path);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    configure_probe_process_group(&mut command);

    let mut child = command.spawn().ok()?;
    let Some(mut stdout) = child.stdout.take().and_then(ProbeStdout::new) else {
        terminate_probe_process(&mut child);
        return Some(ProbeOutcome::Unavailable);
    };
    let deadline = now() + timeout;
    let status = loop {
        if stdout.drain_available() == Drain::Overflow {
            terminate_probe_process(&mut child);
            return Some(ProbeOutcome::Failed);
        }
        match probe_exited(&mut child) {
            Ok(true) => {
                // The exited child is not reaped yet, so its pid still holds
                // the process group id and the signal reaches only the probe's
                // own group.
                terminate_probe_process_group(child.id());
                match child.wait() {
                    Ok(status) => break status,
                    Err(_) => return Some(ProbeOutcome::Unavailable),
                }
            }
            Ok(false) if now() < deadline => {
                std::thread::sleep(VERSION_PROBE_POLL_INTERVAL);
            }
            Ok(false) => {
                terminate_probe_process(&mut child);
                return Some(ProbeOutcome::TimedOut);
            }
            Err(error) if probe_child_gone(&error) => {
                // The child is no longer waitable, so its pid and group id may
                // already belong to an unrelated process and must not be
                // signalled.
                debug!(%error, "version probe child is no longer waitable");
                return Some(ProbeOutcome::Unavailable);
            }
            Err(error) => {
                debug!(%error, "version probe exit check failed");
                terminate_probe_process(&mut child);
                return Some(ProbeOutcome::Unavailable);
            }
        }
    };
    let Some(output) = stdout.finish() else {
        return Some(ProbeOutcome::Failed);
    };
    Some(if status.success() {
        ProbeOutcome::Output(String::from_utf8_lossy(&output).into_owned())
    } else {
        ProbeOutcome::Failed
    })
}

/// Whether the probe's stdout pipe may hold more output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Drain {
    /// The pipe is empty for now or at end of file, and the cap is not reached.
    Within,
    /// More than [`VERSION_OUTPUT_LIMIT`] bytes were written.
    Overflow,
}

/// The read end of a probe's stdout, bounded to [`VERSION_OUTPUT_LIMIT`] bytes.
///
/// The pipe is read without blocking while the probe runs, so a probe that
/// floods its output is noticed within one poll interval and a descendant that
/// keeps the pipe open past the probe's exit cannot stall the daemon.
struct ProbeStdout {
    pipe: ChildStdout,
    bytes: Vec<u8>,
    nonblocking: bool,
}

impl ProbeStdout {
    /// Wraps `pipe`, or `None` when it cannot be read without blocking.
    fn new(pipe: ChildStdout) -> Option<Self> {
        let nonblocking = set_nonblocking(&pipe).ok()?;
        Some(Self {
            pipe,
            bytes: Vec::with_capacity(VERSION_OUTPUT_LIMIT),
            nonblocking,
        })
    }

    /// Moves what the pipe holds right now into the retained output.
    ///
    /// A platform without non-blocking pipe reads leaves the read to
    /// [`Self::finish`], after the probe has exited.
    fn drain_available(&mut self) -> Drain {
        if !self.nonblocking {
            return Drain::Within;
        }
        let mut chunk = [0_u8; VERSION_OUTPUT_LIMIT];
        loop {
            match self.pipe.read(&mut chunk) {
                // End of file, or nothing more to read yet.
                Ok(0) => return Drain::Within,
                Ok(read) => {
                    self.bytes.extend_from_slice(&chunk[..read]);
                    if self.bytes.len() > VERSION_OUTPUT_LIMIT {
                        return Drain::Overflow;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    return Drain::Within;
                }
                Err(error) => {
                    debug!(%error, "version probe output read failed");
                    return Drain::Within;
                }
            }
        }
    }

    /// The retained output once the probe has exited, or `None` when it
    /// exceeded the cap or could not be read.
    fn finish(mut self) -> Option<Vec<u8>> {
        if self.nonblocking {
            return (self.drain_available() == Drain::Within).then_some(self.bytes);
        }
        // A blocking pipe is read only now: the probe's process group is gone,
        // so end of file arrives once the pipe's writers have closed.
        let limit = u64::try_from(VERSION_OUTPUT_LIMIT + 1).ok()?;
        (&mut self.pipe)
            .take(limit)
            .read_to_end(&mut self.bytes)
            .ok()?;
        (self.bytes.len() <= VERSION_OUTPUT_LIMIT).then_some(self.bytes)
    }
}

/// Switches `pipe` to non-blocking reads; `Ok(false)` where the platform
/// has none.
#[cfg(unix)]
fn set_nonblocking(pipe: &ChildStdout) -> std::io::Result<bool> {
    use rustix::fs::{fcntl_getfl, fcntl_setfl, OFlags};

    let flags = fcntl_getfl(pipe)?;
    fcntl_setfl(pipe, flags | OFlags::NONBLOCK)?;
    Ok(true)
}

#[cfg(not(unix))]
fn set_nonblocking(_pipe: &ChildStdout) -> std::io::Result<bool> {
    Ok(false)
}

/// Owner-private state paths for one version probe.
#[derive(Debug)]
struct VersionProbeSandbox {
    dir: PathBuf,
    home_path: PathBuf,
    xdg_config_path: PathBuf,
    xdg_cache_path: PathBuf,
    xdg_data_path: PathBuf,
    xdg_state_path: PathBuf,
    xdg_runtime_path: PathBuf,
    tmp_path: PathBuf,
    cwd_path: PathBuf,
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
                        xdg_config_path: dir.join("xdg-config"),
                        xdg_cache_path: dir.join("xdg-cache"),
                        xdg_data_path: dir.join("xdg-data"),
                        xdg_state_path: dir.join("xdg-state"),
                        xdg_runtime_path: dir.join("xdg-runtime"),
                        tmp_path: dir.join("tmp"),
                        cwd_path: dir.join("cwd"),
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

    fn private_dirs(&self) -> [&Path; 8] {
        [
            &self.home_path,
            &self.xdg_config_path,
            &self.xdg_cache_path,
            &self.xdg_data_path,
            &self.xdg_state_path,
            &self.xdg_runtime_path,
            &self.tmp_path,
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
fn configure_probe_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_probe_process_group(_command: &mut Command) {}

/// Whether the probe child has exited, observed without reaping it, so its pid
/// and process group id stay reserved until `child` is waited.
///
/// An interrupted check reports the child as still running, so the caller's
/// deadline decides when to check again.
#[cfg(unix)]
fn probe_exited(child: &mut std::process::Child) -> std::io::Result<bool> {
    use rustix::process::{waitid, Pid, WaitId, WaitIdOptions};

    let pid = i32::try_from(child.id())
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid probe PID")
        })?;
    match waitid(
        WaitId::Pid(pid),
        WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
    ) {
        Ok(status) => Ok(status.is_some()),
        Err(rustix::io::Errno::INTR) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Whether the probe child has exited.
#[cfg(not(unix))]
fn probe_exited(child: &mut std::process::Child) -> std::io::Result<bool> {
    child.try_wait().map(|status| status.is_some())
}

/// Whether a failed exit check means the probe child is no longer waitable.
#[cfg(unix)]
fn probe_child_gone(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::ECHILD)
}

/// Whether a failed exit check means the probe child is no longer waitable.
#[cfg(not(unix))]
fn probe_child_gone(_error: &std::io::Error) -> bool {
    false
}

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
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;

    use pohunek_test_support::wait::{poll_until, HANG_GUARD};

    use super::*;
    use crate::agent::host::fixture::{official_host, official_profile_pin, OfficialFixtureSource};
    use crate::agent::host::{BuiltinSource, DefinitionOrigin};
    use crate::procwatch::{HostInspector, Pid, ProcessFact, ProcessIdentity, ProcessInspector};

    /// How long fixture processes block; well beyond `HANG_GUARD` so a fixture
    /// can only end through the probe, never by exiting on its own.
    const FIXTURE_HOLD: Duration = Duration::from_secs(HANG_GUARD.as_secs() * 5);

    /// The daemon's own `PATH`, as a launch without a profile override sees it.
    fn process_path() -> Option<OsString> {
        std::env::var_os(SEARCH_PATH_VAR)
    }

    /// The base environment the daemon forwards to agents in this process.
    fn process_base() -> BaseEnv {
        crate::runtime::environment::base_environment(
            pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST,
            &crate::runtime::environment::EnvironmentSource::Process,
        )
        .expect("the default allowlist builds")
    }

    /// An empty profile registry (no host-config layer) for the base-kind tests.
    fn no_profiles() -> ProfileRegistry {
        ProfileRegistry::default()
    }

    fn temp_agents_dir() -> crate::test_support::ScopedDir {
        crate::test_support::scoped_dir("pohunek-caps-")
    }

    #[test]
    fn shell_runtime_is_always_available() {
        let caps = host_capabilities_uncached("0.0.0", &no_profiles(), &process_base());
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
    fn host_capabilities_enumerates_resolvable_profiles_and_probes_their_program() {
        let dir = temp_agents_dir();
        // A profile whose program is a real executable so availability is true.
        std::fs::write(
            dir.join("my-claude.toml"),
            format!(
                "base = \"claude\"\n{}program = \"/bin/sh\"\n",
                official_profile_pin("claude")
            ),
        )
        .expect("write profile");
        let caps = host_capabilities_uncached(
            "0.0.0",
            &ProfileRegistry::with_runtimes(Some(dir.clone()), official_host()),
            &process_base(),
        );

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

    /// The `hermes` definition the official Hermes package serves, with its
    /// version probe.
    fn hermes_definition() -> Arc<RuntimeDefinition> {
        let registry = test_registry();
        Arc::clone(
            registry
                .resolve(&RuntimeId::hermes())
                .expect("hermes registered"),
        )
    }

    fn probe_hermes_for_test(binary: &str) -> AgentRuntime {
        probe_versioned_runtime(
            "hermes",
            RuntimeRef::hermes(),
            binary,
            &hermes_definition(),
            process_path().as_deref(),
        )
    }

    const SEMVER_PROBE: &str =
        r#"{ parser = "semver-v1", args = ["--version"], min = "1.0.0", below = "1.1.0" }"#;

    /// Registry of the built-in shell and the official Codex, Claude and
    /// Hermes packages, whose shell runtime launches a fixed program, so the
    /// inventory does not depend on the host's `$SHELL`.
    fn test_registry() -> RuntimeRegistry {
        RuntimeRegistry::from_sources(&[&BuiltinSource::new("/bin/sh"), &OfficialFixtureSource])
            .expect("the fixture registry builds")
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
            let registry = RuntimeRegistry::from_sources(&[
                &BuiltinSource::from_login_shell(shell),
                &OfficialFixtureSource,
            ])
            .expect("an invalid login shell must not fail the registry");
            let caps =
                host_capabilities_for_uncached("0.0.0", &no_profiles(), &registry, &process_base());
            let names: Vec<&str> = caps.runtimes.iter().map(|r| r.agent.as_str()).collect();
            assert_eq!(names, RESERVED_RUNTIME_IDS);
            let shell_def = registry
                .resolve(&RuntimeId::parse("shell").expect("id"))
                .expect("shell registered");
            assert_eq!(shell_def.program().as_str(), "/bin/sh");
            registry
                .resolve(&RuntimeId::hermes())
                .expect("hermes registered");
        }
        let valid = BuiltinSource::from_login_shell(Some("/usr/bin/zsh".to_owned()));
        let registry = RuntimeRegistry::from_sources(&[&valid]).expect("registry");
        let shell_def = registry
            .resolve(&RuntimeId::parse("shell").expect("id"))
            .expect("shell registered");
        assert_eq!(shell_def.program().as_str(), "/usr/bin/zsh");
    }

    #[test]
    fn inventory_lists_reserved_runtimes_in_reserved_order_with_their_wire_shape() {
        let caps = host_capabilities_for_uncached(
            "0.0.0",
            &no_profiles(),
            &test_registry(),
            &process_base(),
        );
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
        // Only a runtime with a version-probe parser reports a policy; the
        // official fixture keeps only the Hermes package's probe.
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
            process_path().as_deref(),
        );
        assert!(runtime.available);
        assert_eq!(runtime.version, None);
        assert_eq!(runtime.supported, Some(false));
    }

    #[test]
    fn profile_over_hermes_base_is_probed_with_the_hermes_parser() {
        let dir = temp_agents_dir();
        let hermes = dir.join("hermes-bin");
        write_test_executable(&hermes, "#!/bin/sh\necho 'Hermes Agent v1.0.0 (future)'\n");
        std::fs::write(
            dir.join("hermes-review.toml"),
            format!(
                "base = \"hermes\"\n{}program = \"{}\"\n",
                official_profile_pin("hermes"),
                hermes.display()
            ),
        )
        .expect("write profile");
        let caps = host_capabilities_for_uncached(
            "0.0.0",
            &ProfileRegistry::with_runtimes(Some(dir.clone()), official_host()),
            &test_registry(),
            &process_base(),
        );
        let runtime = caps
            .runtimes
            .iter()
            .find(|runtime| runtime.agent == "hermes-review")
            .expect("profile runtime reported");
        assert_eq!(runtime.agent_base, Some(RuntimeRef::hermes()));
        assert_eq!(runtime.version.as_deref(), Some("1.0.0"));
        assert_eq!(runtime.supported, Some(false));
    }

    #[test]
    fn hermes_inventory_distinguishes_supported_unsupported_and_missing() {
        let dir = temp_agents_dir();
        let hermes = dir.join("hermes");

        write_test_executable(
            &hermes,
            "#!/bin/sh\necho 'Hermes Agent v0.20.0 (2026.8.3)'\n",
        );
        let supported = probe_hermes_for_test(&hermes.display().to_string());
        assert!(supported.available);
        assert_eq!(supported.agent_base, Some(RuntimeRef::hermes()));
        assert_eq!(supported.version.as_deref(), Some("0.20.0"));
        assert_eq!(supported.supported, Some(true));

        write_test_executable(&hermes, "#!/bin/sh\necho 'Hermes Agent v1.0.0 (future)'\n");
        let wrong = probe_hermes_for_test(&hermes.display().to_string());
        assert!(wrong.available);
        assert_eq!(wrong.version.as_deref(), Some("1.0.0"));
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
        let definition = hermes_definition();
        let validate = |binary: &str| {
            validate_definition_launch(
                &definition,
                Spawn::Program(binary),
                process_path().as_deref(),
            )
        };

        write_test_executable(
            &hermes,
            "#!/bin/sh\necho 'Hermes Agent v0.20.0 (2026.8.3)'\n",
        );
        let validated = validate(&binary)
            .expect("pinned Hermes runtime")
            .expect("Hermes has a validated program");
        assert_eq!(
            validated.as_path(),
            hermes.canonicalize().expect("canonical Hermes fixture")
        );

        // The range runs up to the next major: a later 0.x release launches.
        write_test_executable(
            &hermes,
            "#!/bin/sh\necho 'Hermes Agent v0.21.0 (2026.9.1)'\n",
        );
        validate(&binary)
            .expect("a later 0.x Hermes release")
            .expect("Hermes has a validated program");

        // An out-of-range release is named with the declared range; output
        // that does not parse is never echoed.
        for (output, expected, forbidden) in [
            (
                "Hermes Agent v1.0.0 (future)",
                "reports version 1.0.0, outside its runtime's supported range",
                "(future)",
            ),
            (
                "unexpected output",
                "does not match its runtime's declared format",
                "unexpected output",
            ),
        ] {
            write_test_executable(&hermes, &format!("#!/bin/sh\necho '{output}'\n"));
            let error = validate(&binary).expect_err("incompatible Hermes runtime");
            assert_eq!(error.code, "agent_runtime_unsupported");
            assert!(error.msg.contains(expected), "{error:?}");
            assert!(!error.msg.contains(forbidden), "{error:?}");
            assert!(!error.msg.contains(&binary));
            assert!(error.recover.is_some(), "{error:?}");
        }

        let missing = dir.join("missing").display().to_string();
        let error = validate(&missing).expect_err("missing Hermes runtime");
        assert_eq!(error.code, "agent_binary_missing");
        assert!(!error.msg.contains(&missing));
    }

    /// Runs `path` as the probe of [`SEMVER_PROBE`] under the daemon's own
    /// `PATH`, with `seeded_env` set before the probe clears the environment.
    fn run_semver_probe(
        path: &Path,
        timeout: Duration,
        seeded_env: &[(&str, &str)],
        now: &impl Fn() -> Instant,
    ) -> ProbeOutcome {
        let definition = probed_definition(SEMVER_PROBE);
        let policy = definition.version_probe_policy().expect("policy");
        spawn_and_collect_probe(
            path,
            policy,
            process_path().as_deref(),
            &[],
            timeout,
            seeded_env,
            now,
        )
        .unwrap_or(ProbeOutcome::Unavailable)
    }

    #[test]
    fn version_probe_clears_ambient_state_and_isolates_all_writable_paths() {
        let dir = temp_agents_dir();
        let tool = dir.join("tool");
        let marker = dir.join("isolation.txt");
        write_test_executable(
            &tool,
            &format!(
                "#!/bin/sh\n\
                 [ \"${{POHUNEK_PROBE_SENTINEL+x}}\" != x ] || exit 20\n\
                 [ \"${{PYTHONPATH+x}}\" != x ] || exit 21\n\
                 [ \"${{PYTHONHOME+x}}\" != x ] || exit 22\n\
                 [ \"${{VIRTUAL_ENV+x}}\" != x ] || exit 23\n\
                 [ \"${{CONDA_PREFIX+x}}\" != x ] || exit 24\n\
                 [ \"${{UV_PROJECT_ENVIRONMENT+x}}\" != x ] || exit 25\n\
                 [ \"$PATH\" = '{probe_path}' ] || exit 26\n\
                 [ \"${{HERMES_HOME+x}}\" != x ] || exit 27\n\
                 [ \"${{HOME##*/}}\" = home ] || exit 30\n\
                 [ \"${{XDG_CONFIG_HOME##*/}}\" = xdg-config ] || exit 32\n\
                 [ \"${{XDG_CACHE_HOME##*/}}\" = xdg-cache ] || exit 33\n\
                 [ \"${{XDG_DATA_HOME##*/}}\" = xdg-data ] || exit 34\n\
                 [ \"${{XDG_STATE_HOME##*/}}\" = xdg-state ] || exit 35\n\
                 [ \"${{XDG_RUNTIME_DIR##*/}}\" = xdg-runtime ] || exit 36\n\
                 [ \"${{TMPDIR##*/}}\" = tmp ] || exit 37\n\
                 probe_cwd=$(pwd) || exit 39\n\
                 [ \"${{probe_cwd##*/}}\" = cwd ] || exit 40\n\
                 for private_dir in \"$HOME\" \"$XDG_CONFIG_HOME\" \
                   \"$XDG_CACHE_HOME\" \"$XDG_DATA_HOME\" \"$XDG_STATE_HOME\" \
                   \"$XDG_RUNTIME_DIR\" \"$TMPDIR\"; do\n\
                   [ -d \"$private_dir\" ] || exit 41\n\
                 done\n\
                 printf 'isolated' > {marker}\n\
                 echo 1.0.0\n",
                marker = marker.display(),
                probe_path = std::env::var(SEARCH_PATH_VAR).expect("the test process has a PATH"),
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
        let outcome = run_semver_probe(&tool, VERSION_PROBE_TIMEOUT, &seeded_env, &Instant::now);
        assert!(matches!(outcome, ProbeOutcome::Output(_)), "{outcome:?}");
        assert_eq!(
            std::fs::read_to_string(marker).expect("isolation marker"),
            "isolated"
        );
    }

    #[test]
    fn version_probe_directories_are_owner_private() {
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
    fn version_probe_honors_its_timeout_argument() {
        const STEPS: u32 = 4;
        let dir = temp_agents_dir();
        let tool = dir.join("tool");
        write_test_executable(
            &tool,
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

        let outcome = run_semver_probe(&tool, timeout, &[], &clock);

        assert!(matches!(outcome, ProbeOutcome::TimedOut), "{outcome:?}");
        assert_eq!(
            reads.get(),
            STEPS + 1,
            "the probe must expire when the clock reaches start + timeout"
        );
    }

    #[test]
    fn version_probe_timeout_kills_descendants() {
        let dir = temp_agents_dir();
        let tool = dir.join("tool");
        let pid_file = dir.join("descendant.pid");
        // Both sleepers are started before the descendant records its pid, so
        // the whole tree exists when the test observes it.
        write_test_executable(
            &tool,
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
        let outcome = run_semver_probe(&tool, timeout, &[], &clock);
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
    fn version_probe_output_is_bounded() {
        let dir = temp_agents_dir();
        let tool = dir.join("tool");
        write_test_executable(&tool, "#!/bin/sh\nhead -c 1048576 /dev/zero\nexit 0\n");

        // The deadline is far beyond any plausible scheduling delay, and the
        // script exits successfully, so a `Failed` outcome can only come from
        // the output limit ending the probe, never from its own timeout or exit
        // status.
        let outcome = run_semver_probe(&tool, HANG_GUARD, &[], &Instant::now);

        assert!(matches!(outcome, ProbeOutcome::Failed), "{outcome:?}");
    }

    #[test]
    fn version_probe_output_at_the_limit_is_kept() {
        let dir = temp_agents_dir();
        let tool = dir.join("tool");
        write_test_executable(
            &tool,
            &format!("#!/bin/sh\nhead -c {VERSION_OUTPUT_LIMIT} /dev/zero | tr '\\0' '1'\n"),
        );

        let outcome = run_semver_probe(&tool, HANG_GUARD, &[], &Instant::now);

        assert!(
            matches!(&outcome, ProbeOutcome::Output(output) if output.len() == VERSION_OUTPUT_LIMIT),
            "{outcome:?}"
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
                "base = \"hermes\"\n{}program = \"{}\"\nargs = [\"chat\"]\n",
                official_profile_pin("hermes"),
                non_executable.display()
            ),
        )
        .expect("write profile");

        let caps = host_capabilities_uncached(
            "0.0.0",
            &ProfileRegistry::with_runtimes(Some(dir.clone()), official_host()),
            &process_base(),
        );
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
        let supported = probe_versioned_runtime(
            "tool",
            RuntimeRef::from_wire("tool"),
            &binary,
            &definition,
            process_path().as_deref(),
        );
        assert!(supported.available);
        assert_eq!(supported.version.as_deref(), Some("1.0.2"));
        assert_eq!(supported.supported, Some(true));
        let validated = validate_definition_launch(
            &definition,
            Spawn::Program(&binary),
            process_path().as_deref(),
        )
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
                process_path().as_deref(),
            );
            assert!(runtime.available, "{output:?}");
            assert_eq!(runtime.version.as_deref(), version, "{output:?}");
            assert_eq!(runtime.supported, Some(false), "{output:?}");
            let error = validate_definition_launch(
                &definition,
                Spawn::Program(&binary),
                process_path().as_deref(),
            )
            .expect_err("a release outside the range is refused");
            assert_eq!(error.code, "agent_runtime_unsupported");
            match version {
                Some(version) => assert!(
                    error
                        .msg
                        .contains(&format!("reports version {version}, outside")),
                    "{error:?}"
                ),
                None => assert!(!error.msg.contains(output) || output.is_empty()),
            }
        }

        write_test_executable(&tool, "#!/bin/sh\necho 1.0.2\nexit 3\n");
        let failing = probe_versioned_runtime(
            "tool",
            RuntimeRef::from_wire("tool"),
            &binary,
            &definition,
            process_path().as_deref(),
        );
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
            process_path().as_deref(),
        );
        assert!(!missing.available);
        assert_eq!(missing.supported, None);
    }

    /// Two interpreter directories whose `interp` shim prints a release inside
    /// (`in_range`) and outside (`out_of_range`) the probe range, and a script
    /// that runs whichever `interp` the `PATH` it is started under finds.
    struct InterpreterFixture {
        _root: crate::test_support::ScopedDir,
        tool: PathBuf,
        in_range: PathBuf,
        out_of_range: PathBuf,
    }

    fn interpreter_fixture() -> InterpreterFixture {
        let root = temp_agents_dir();
        let in_range = root.join("in-range");
        let out_of_range = root.join("out-of-range");
        for (dir, version) in [(&in_range, "1.0.2"), (&out_of_range, "2.0.0")] {
            std::fs::create_dir(dir).expect("interpreter dir");
            write_test_executable(&dir.join("interp"), &format!("#!/bin/sh\necho {version}\n"));
        }
        let tool = root.join("tool");
        write_test_executable(&tool, "#!/usr/bin/env interp\n");
        InterpreterFixture {
            tool,
            in_range,
            out_of_range,
            _root: root,
        }
    }

    fn version_under(
        fixture: &InterpreterFixture,
        definition: &RuntimeDefinition,
        launch_path: &Path,
    ) -> Option<String> {
        probe_versioned_runtime(
            "tool",
            RuntimeRef::from_wire("tool"),
            &fixture.tool.display().to_string(),
            definition,
            Some(launch_path.as_os_str()),
        )
        .version
    }

    #[test]
    fn semver_probe_resolves_the_interpreter_on_the_launch_path() {
        let fixture = interpreter_fixture();
        let definition = probed_definition(SEMVER_PROBE);

        assert_eq!(
            version_under(&fixture, &definition, &fixture.in_range).as_deref(),
            Some("1.0.2")
        );
        assert_eq!(
            version_under(&fixture, &definition, &fixture.out_of_range).as_deref(),
            Some("2.0.0")
        );
        let binary = fixture.tool.display().to_string();
        assert!(
            validate_definition_launch(
                &definition,
                Spawn::Program(&binary),
                Some(fixture.in_range.as_os_str())
            )
            .is_ok(),
            "an interpreter inside the range launches"
        );
        let error = validate_definition_launch(
            &definition,
            Spawn::Program(&binary),
            Some(fixture.out_of_range.as_os_str()),
        )
        .expect_err("an interpreter outside the range is refused");
        assert_eq!(error.code, "agent_runtime_unsupported");
    }

    #[test]
    fn a_profile_path_override_decides_the_interpreter_the_inventory_probes() {
        let fixture = interpreter_fixture();
        let definition = probed_definition(SEMVER_PROBE);
        let binary = fixture.tool.display().to_string();
        let daemon_base = |dir: &Path| {
            BaseEnv::new(std::collections::BTreeMap::from([(
                "PATH".to_owned(),
                dir.display().to_string(),
            )]))
            .expect("base environment")
        };
        let profile_path = |dir: &Path| vec![("PATH".to_owned(), dir.display().to_string())];
        let supported = |base: &BaseEnv, profile_env: &[(String, String)]| {
            probed_entry(probe_definition(
                "tool",
                RuntimeRef::from_wire("tool"),
                &definition,
                &binary,
                base,
                profile_env,
            ))
            .supported
        };

        // The profile PATH wins over the daemon's, in both directions.
        assert_eq!(
            supported(
                &daemon_base(&fixture.out_of_range),
                &profile_path(&fixture.in_range)
            ),
            Some(true)
        );
        assert_eq!(
            supported(
                &daemon_base(&fixture.in_range),
                &profile_path(&fixture.out_of_range)
            ),
            Some(false)
        );
        // Without an override the base environment's PATH decides.
        assert_eq!(supported(&daemon_base(&fixture.in_range), &[]), Some(true));
        assert_eq!(
            supported(&daemon_base(&fixture.out_of_range), &[]),
            Some(false)
        );
        // A launch without any PATH has no interpreter to find.
        assert_eq!(supported(&BaseEnv::default(), &[]), Some(false));
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
            policy,
            process_path().as_deref(),
            &[],
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
    fn line_probe_reports_each_official_banner_inside_the_declared_range() {
        let dir = temp_agents_dir();
        let tool = dir.join("tool");
        let binary = tool.display().to_string();
        for (line, banner, version) in [
            ("codex-cli {version}", "codex-cli 0.160.0", "0.160.0"),
            (
                "{version} (Claude Code)",
                "2.1.289 (Claude Code)",
                "2.1.289",
            ),
            (
                "Hermes Agent v{version} {annotation}",
                "Hermes Agent v0.20.0 (2026.8.3)\nProject: /x",
                "0.20.0",
            ),
        ] {
            let definition = probed_definition(&format!(
                r#"{{ parser = "semver-line-v1", args = ["--version"], min = "0.1.0", below = "3.0.0", line = "{line}" }}"#
            ));
            write_test_executable(&tool, &format!("#!/bin/sh\nprintf '%s\\n' '{banner}'\n"));
            let runtime = probe_versioned_runtime(
                "tool",
                RuntimeRef::from_wire("tool"),
                &binary,
                &definition,
                process_path().as_deref(),
            );
            assert_eq!(runtime.version.as_deref(), Some(version), "{line}");
            assert_eq!(runtime.supported, Some(true), "{line}");
            assert!(
                validate_definition_launch(
                    &definition,
                    Spawn::Program(&binary),
                    process_path().as_deref()
                )
                .is_ok(),
                "{line}"
            );

            let below = probed_definition(&format!(
                r#"{{ parser = "semver-line-v1", args = ["--version"], min = "0.1.0", below = "0.2.0", line = "{line}" }}"#
            ));
            let out_of_range = probe_versioned_runtime(
                "tool",
                RuntimeRef::from_wire("tool"),
                &binary,
                &below,
                process_path().as_deref(),
            );
            assert_eq!(out_of_range.version.as_deref(), Some(version), "{line}");
            assert_eq!(out_of_range.supported, Some(false), "{line}");
        }

        let definition = probed_definition(
            r#"{ parser = "semver-line-v1", args = ["--version"], min = "0.1.0", below = "3.0.0", line = "codex-cli {version}" }"#,
        );
        for banner in ["2.1.289 (Claude Code)", "codex-cli 0.160.0-rc.1", ""] {
            write_test_executable(&tool, &format!("#!/bin/sh\necho '{banner}'\n"));
            let runtime = probe_versioned_runtime(
                "tool",
                RuntimeRef::from_wire("tool"),
                &binary,
                &definition,
                process_path().as_deref(),
            );
            assert_eq!(runtime.version, None, "{banner:?}");
            assert_eq!(runtime.supported, Some(false), "{banner:?}");
            let error = validate_definition_launch(
                &definition,
                Spawn::Program(&binary),
                process_path().as_deref(),
            )
            .expect_err("an unreadable banner is refused");
            assert_eq!(error.code, "agent_runtime_unsupported");
        }
    }

    #[test]
    fn line_probe_descriptors_are_validated_with_the_package() {
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
        let common = r#"args = ["--version"], min = "1.0.0", below = "2.0.0""#;
        parse(&format!(
            r#"{{ parser = "semver-line-v1", {common}, line = "codex-cli {{version}}" }}"#
        ))
        .expect("a valid template parses");

        let rejected = [
            // The parser needs its template; the bare parser takes none.
            format!(r#"{{ parser = "semver-line-v1", {common} }}"#),
            format!(r#"{{ parser = "semver-v1", {common}, line = "codex-cli {{version}}" }}"#),
            format!(r#"{{ parser = "not-compiled", {common}, line = "codex-cli {{version}}" }}"#),
            r#"{ parser = "semver-line-v1", line = "codex-cli {version}" }"#.to_owned(),
            // Over-broad or unparsable templates.
            format!(r#"{{ parser = "semver-line-v1", {common}, line = "{{version}}" }}"#),
            format!(r#"{{ parser = "semver-line-v1", {common}, line = "" }}"#),
            format!(r#"{{ parser = "semver-line-v1", {common}, line = "codex-cli" }}"#),
            format!(r#"{{ parser = "semver-line-v1", {common}, line = "a {{version}} {{x}}" }}"#),
            format!(
                r#"{{ parser = "semver-line-v1", {common}, line = "a {{version}} {{annotation}} b" }}"#
            ),
            // Unknown keys stay refused.
            format!(
                r#"{{ parser = "semver-line-v1", {common}, line = "a {{version}}", regex = "x" }}"#
            ),
        ];
        for invalid in &rejected {
            assert!(
                matches!(
                    parse(invalid),
                    Err(crate::agent::host::DefinitionError::Field { .. }
                        | crate::agent::host::DefinitionError::Malformed { .. })
                ),
                "{invalid}"
            );
        }
        let field = |probe: &str| match parse(probe) {
            Err(crate::agent::host::DefinitionError::Field { field, .. }) => field,
            other => panic!("expected a field error for {probe}, got {other:?}"),
        };
        assert_eq!(
            field(&format!(
                r#"{{ parser = "semver-line-v1", {common}, line = "{{version}}" }}"#
            )),
            "runtime.version_probe.line"
        );
        assert_eq!(
            field(&format!(r#"{{ parser = "semver-line-v1", {common} }}"#)),
            "runtime.version_probe.line"
        );
        assert_eq!(
            field(&format!(
                r#"{{ parser = "semver-v1", {common}, line = "a {{version}}" }}"#
            )),
            "runtime.version_probe.line"
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
            r#"{ parser = "not-compiled", args = ["--version"], min = "1.0.0", below = "1.1.0" }"#,
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
        parse(r#"{ parser = "not-compiled" }"#)
            .expect("a parser id the daemon does not compile in parses; a launch refuses it");
    }
}

#[cfg(test)]
mod config_home_tests;
