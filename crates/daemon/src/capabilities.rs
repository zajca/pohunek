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
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use protocol::{AgentKind, AgentRuntime, HostCapabilities, ProtocolError, PROTOCOL_VERSION};
use serde::Deserialize;

use crate::agent::{
    default_program, is_executable_file, which_executable, ProfileRegistry, ValidatedLaunchProgram,
};

/// The reviewed Hermes release metadata shipped with this Pohunek build.
const HERMES_COMPATIBILITY_LOCK: &str =
    include_str!("../../../compat/hermes/compatibility-lock.json");
/// Maximum wall time for the local `hermes --version` inventory probe.
const HERMES_VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Maximum stdout the version probe may write or retain.
const HERMES_VERSION_OUTPUT_LIMIT: usize = 4 * 1024;
/// Maximum normalized version length accepted from untrusted probe output.
const MAX_HERMES_VERSION_BYTES: usize = 64;
/// Poll cadence balances bounded shutdown with negligible idle CPU use.
const VERSION_PROBE_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// A deterministic executable search path avoids inheriting user shims or hooks.
const HERMES_PROBE_PATH: &str = "/usr/bin:/bin";
/// A deterministic locale keeps provider version output stable.
const HERMES_PROBE_LOCALE: &str = "C";

/// Build the live capability snapshot for this host.
///
/// `supported_agents` is the four compiled base kinds plus every resolvable host
/// agent profile (Part C). `runtimes` reports, per agent, whether its backing
/// program is present on `PATH`: the shell runtime is always available (no path),
/// `codex`/`claude`/`hermes` are probed, and each profile probes its (possibly-overridden)
/// program. Probing uses the same executable check as the launch path
/// ([`which_executable`]) so "available" agrees with what a launch would accept.
/// `git_available` reflects a `git` probe and currently also gates worktree support.
#[must_use]
pub(crate) fn host_capabilities(
    daemon_version: &str,
    profiles: &ProfileRegistry,
) -> HostCapabilities {
    let mut supported_agents = vec![
        "shell".to_owned(),
        "codex".to_owned(),
        "claude".to_owned(),
        "hermes".to_owned(),
    ];

    let mut runtimes = vec![
        // The shell runtime is always available; the daemon falls back to a
        // login shell and does not require a named binary on PATH.
        AgentRuntime {
            agent: "shell".to_owned(),
            agent_base: Some(AgentKind::Shell),
            available: true,
            path: None,
            version: None,
            supported: None,
        },
        probe_runtime("codex", AgentKind::Codex, "codex"),
        probe_runtime("claude", AgentKind::Claude, "claude"),
        probe_hermes_runtime("hermes", "hermes"),
    ];

    // Host profiles: each resolvable profile is a launchable agent name; probe its
    // resolved program exactly as a base kind, so availability is consistent.
    for agent in profiles.enumerate() {
        let program = agent.profile.as_ref().map_or_else(
            || default_program(&agent.base),
            |profile| profile.program.clone(),
        );
        runtimes.push(if agent.base == AgentKind::Hermes {
            probe_hermes_runtime(&agent.name, &program)
        } else {
            probe_runtime(&agent.name, agent.base, &program)
        });
        supported_agents.push(agent.name);
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

/// Probe `PATH` for an agent's backing program and build its runtime entry.
///
/// `available` is true exactly when the program resolves to an **executable** on
/// `PATH` (the launch-path check, [`which_executable`]); the resolved path is
/// reported when found.
fn probe_runtime(agent: &str, agent_base: AgentKind, binary: &str) -> AgentRuntime {
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

/// Probe a Hermes executable and classify its version without exposing output.
fn probe_hermes_runtime(agent: &str, binary: &str) -> AgentRuntime {
    let Some(program) = ValidatedLaunchProgram::resolve(binary) else {
        return AgentRuntime {
            agent: agent.to_owned(),
            agent_base: Some(AgentKind::Hermes),
            available: false,
            path: None,
            version: None,
            supported: None,
        };
    };

    let version = run_hermes_version_probe(program.as_path())
        .and_then(|output| parse_hermes_version(&output));
    let supported = version.as_deref() == Some(supported_hermes_version());
    AgentRuntime {
        agent: agent.to_owned(),
        agent_base: Some(AgentKind::Hermes),
        available: true,
        path: Some(program.as_path().display().to_string()),
        version,
        supported: Some(supported),
    }
}

/// Validates a launch runtime and pins the resolved Hermes executable path.
///
/// Non-Hermes adapters retain their existing launch behavior. Hermes resolves
/// the executable once, probes it in the same isolated bounded sandbox used by
/// inventory, and returns that exact path only for the pinned supported release.
/// Probe output, configured paths, and detected versions never enter the error.
pub(crate) fn validate_launch_runtime(
    base: &AgentKind,
    binary: &str,
) -> Result<Option<ValidatedLaunchProgram>, ProtocolError> {
    if *base != AgentKind::Hermes {
        return Ok(None);
    }

    let program = ValidatedLaunchProgram::resolve(binary)
        .ok_or_else(ProtocolError::agent_runtime_unsupported)?;
    let supported = run_hermes_version_probe(program.as_path())
        .and_then(|output| parse_hermes_version(&output))
        .is_some_and(|version| version == supported_hermes_version());
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
    run_hermes_version_probe_with_timeout(path, HERMES_VERSION_PROBE_TIMEOUT)
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
        .arg("--version")
        .current_dir(&sandbox.cwd_path)
        .env("PATH", HERMES_PROBE_PATH)
        .env("LANG", HERMES_PROBE_LOCALE)
        .env("LC_ALL", HERMES_PROBE_LOCALE)
        .env("HOME", &sandbox.home_path)
        .env("HERMES_HOME", &sandbox.hermes_home_path)
        .env("XDG_CONFIG_HOME", &sandbox.xdg_config_path)
        .env("XDG_CACHE_HOME", &sandbox.xdg_cache_path)
        .env("XDG_DATA_HOME", &sandbox.xdg_data_path)
        .env("XDG_STATE_HOME", &sandbox.xdg_state_path)
        .env("XDG_RUNTIME_DIR", &sandbox.xdg_runtime_path)
        .env("TMPDIR", &sandbox.tmp_path)
        .env("PYTHONNOUSERSITE", "1")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("PYTHONSAFEPATH", "1")
        .env("PYTHONPYCACHEPREFIX", &sandbox.python_cache_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output_file))
        .stderr(Stdio::null());
    configure_probe_process_group(&mut command);

    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(VERSION_PROBE_POLL_INTERVAL);
            }
            Ok(None) | Err(_) => {
                terminate_probe_process(&mut child);
                return None;
            }
        }
    };
    terminate_probe_process_group(child.id());
    let mut output = Vec::with_capacity(HERMES_VERSION_OUTPUT_LIMIT);
    File::open(&sandbox.output_path)
        .ok()?
        .take(u64::try_from(HERMES_VERSION_OUTPUT_LIMIT).ok()?)
        .read_to_end(&mut output)
        .ok()?;
    status
        .success()
        .then(|| String::from_utf8_lossy(&output).into_owned())
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
            let dir = std::env::temp_dir().join(format!(
                "pohunek-hermes-version-probe-{}",
                ulid::Ulid::new()
            ));
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
                rlim_cur: HERMES_VERSION_OUTPUT_LIMIT as libc::rlim_t,
                rlim_max: HERMES_VERSION_OUTPUT_LIMIT as libc::rlim_t,
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
    for dir in std::env::split_paths(path_var) {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// An empty profile registry (no host-config layer) for the base-kind tests.
    fn no_profiles() -> ProfileRegistry {
        ProfileRegistry::default()
    }

    fn temp_agents_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after epoch")
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("pohunek-caps-{}-{nanos}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create agents dir");
        dir
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
        assert_eq!(shell.agent_base, Some(AgentKind::Shell));
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
        let caps = host_capabilities("0.0.0", &ProfileRegistry::new(Some(dir)));

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
        assert_eq!(runtime.agent_base, Some(AgentKind::Claude));
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

    #[test]
    fn hermes_inventory_distinguishes_supported_unsupported_and_missing() {
        let dir = temp_agents_dir();
        let hermes = dir.join("hermes");

        write_test_executable(
            &hermes,
            "#!/bin/sh\necho 'Hermes Agent v0.20.0 (2026-08-03)'\n",
        );
        let supported = probe_hermes_runtime("hermes", &hermes.display().to_string());
        assert!(supported.available);
        assert_eq!(supported.agent_base, Some(AgentKind::Hermes));
        assert_eq!(supported.version.as_deref(), Some("0.20.0"));
        assert_eq!(supported.supported, Some(true));

        write_test_executable(&hermes, "#!/bin/sh\necho 'Hermes Agent v0.21.0 (future)'\n");
        let wrong = probe_hermes_runtime("hermes", &hermes.display().to_string());
        assert!(wrong.available);
        assert_eq!(wrong.version.as_deref(), Some("0.21.0"));
        assert_eq!(wrong.supported, Some(false));

        write_test_executable(&hermes, "#!/bin/sh\necho 'unexpected output'\n");
        let unparseable = probe_hermes_runtime("hermes", &hermes.display().to_string());
        assert!(unparseable.available);
        assert_eq!(unparseable.version, None);
        assert_eq!(unparseable.supported, Some(false));

        let missing = probe_hermes_runtime("hermes", &dir.join("missing").display().to_string());
        assert!(!missing.available);
        assert_eq!(missing.agent_base, Some(AgentKind::Hermes));
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
        let validated = validate_launch_runtime(&AgentKind::Hermes, &binary)
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
            let error = validate_launch_runtime(&AgentKind::Hermes, &binary)
                .expect_err("incompatible Hermes runtime");
            assert_eq!(error.code, "agent_runtime_unsupported");
            assert!(!error.msg.contains(forbidden));
            assert!(!error.msg.contains(&binary));
        }

        let missing = dir.join("missing").display().to_string();
        let error = validate_launch_runtime(&AgentKind::Hermes, &missing)
            .expect_err("missing Hermes runtime");
        assert_eq!(error.code, "agent_runtime_unsupported");
        assert!(!error.msg.contains(&missing));

        assert_eq!(
            validate_launch_runtime(&AgentKind::Claude, &missing)
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
            HERMES_VERSION_PROBE_TIMEOUT,
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
        write_test_executable(&hermes, "#!/bin/sh\nsleep 30\n");
        let started = Instant::now();

        assert!(
            run_hermes_version_probe_with_timeout(&hermes, Duration::from_millis(50)).is_none()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn hermes_version_probe_timeout_kills_descendants() {
        let dir = temp_agents_dir();
        let hermes = dir.join("hermes");
        let descendant_marker = dir.join("descendant.txt");
        write_test_executable(
            &hermes,
            &format!(
                "#!/bin/sh\n(sleep 0.2; printf 'escaped' > {}) &\nsleep 30\n",
                descendant_marker.display()
            ),
        );

        assert!(
            run_hermes_version_probe_with_timeout(&hermes, Duration::from_millis(50)).is_none()
        );
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !descendant_marker.exists(),
            "the timed-out probe process group must be terminated"
        );
    }

    #[test]
    fn hermes_version_probe_output_is_bounded() {
        let dir = temp_agents_dir();
        let hermes = dir.join("hermes");
        write_test_executable(&hermes, "#!/bin/sh\nexec head -c 1048576 /dev/zero\n");
        let started = Instant::now();

        assert!(run_hermes_version_probe(&hermes).is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
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

        let caps = host_capabilities("0.0.0", &ProfileRegistry::new(Some(dir)));
        let runtime = caps
            .runtimes
            .iter()
            .find(|runtime| runtime.agent == "hermes-work")
            .expect("Hermes profile runtime");
        assert!(!runtime.available);
        assert_eq!(runtime.agent_base, Some(AgentKind::Hermes));
        assert_eq!(runtime.supported, None);
    }

    fn write_test_executable(path: &Path, body: &str) {
        std::fs::write(path, body).expect("write executable");
        let mut permissions = std::fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions).expect("set executable mode");
    }
}
