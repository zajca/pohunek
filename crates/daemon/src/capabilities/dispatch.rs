//! Mise shim resolution for version probes.
//!
//! A version-manager shim (a symlink to the `mise` executable) dispatches on
//! its own name: the probe must not run the manager's own `--version`.
//! Instead, `mise which <name>` names the installed executable the shim would
//! run, read-only (`MISE_OFFLINE=1`, `MISE_AUTO_INSTALL=0`) and with only the
//! owner's location variables forwarded. The lookup starts in `/`, whose only
//! ancestor is itself, so mise reads the owner's global/system selection and
//! no project directory above the lookup can change the named tool. The named
//! executable is probed in the private sandbox; the launch still execs the
//! shim link, so the manager and any account dispatch on top of it behave as
//! in the user's shell.

// Rust guideline compliant 2026-11-16

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::bounded_run::{self, HomeScope, RunLimits, RunOutcome, RunSpec, StderrPolicy};
use crate::runtime::environment::effective_variable;
use pohunek_platform::shell_env;
use pohunek_worker_protocol::BaseEnv;

/// File name of the mise executable: symlink shims point at it and dispatch
/// on `argv[0]`, while the manager itself runs when `argv[0]` is this name
/// (`MISE_BIN_NAME = filename(ARGV0)` in `crates/mise-util/src/env.rs`).
const MISE_EXECUTABLE_NAME: &str = "mise";

/// The read-only lookup subcommand: prints the installed executable the shim
/// `<name>` would run, and fails when nothing is installed.
const MISE_WHICH_ARG: &str = "which";

/// Blocks every network request the lookup could make, so the probe stays
/// offline (verified in mise `settings.toml`).
const OFFLINE_ENV: (&str, &str) = ("MISE_OFFLINE", "1");

/// Turns off every automatic install, so the probe never downloads or writes
/// a tool (verified in mise `settings.toml`).
const NO_AUTO_INSTALL_ENV: (&str, &str) = ("MISE_AUTO_INSTALL", "0");

/// Owner location variables the lookup runs with, read through
/// [`effective_variable`] so the launched agent sees the same values. These
/// are directories mise resolves its installs, shims and config from
/// (`crates/mise-util/src/env.rs`): never tokens or credentials.
const LOCATION_VARS: [&str; 12] = [
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "MISE_CONFIG_DIR",
    "MISE_DATA_DIR",
    "MISE_STATE_DIR",
    "MISE_CACHE_DIR",
    "MISE_INSTALLS_DIR",
    "MISE_SYSTEM_CONFIG_DIR",
    "MISE_SYSTEM_DIR",
];

/// Most stdout a `mise which` reply may hold: an absolute path line.
const WHICH_OUTPUT_LIMIT: usize = 4 * 1024;

/// The directory the lookup runs in: `/` is root-owned and has no ancestors,
/// so project config in any directory the lookup would otherwise walk up
/// through cannot change which installed tool mise names. The lookup takes no
/// `MISE_CEILING_PATHS`: the ceiling variable is unnecessary when the only
/// ancestor searched is the root itself.
const LOOKUP_WORKING_DIRECTORY: &str = "/";

/// How a validated launch path dispatches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Dispatch {
    /// A plain executable or any non-mise `argv[0]` link: probed directly.
    Direct,
    /// A mise symlink shim: `manager` is the canonical mise executable run
    /// with `argv[0]` equal to its path (manager mode), and `bin_name` is the
    /// link's own file name the lookup asks about.
    Mise { manager: PathBuf, bin_name: String },
}

/// Sorts a validated launch path into [`Dispatch::Mise`] only when its final
/// component is a symlink whose canonical target is the `mise` executable
/// under a different name; a link named `mise` itself stays [`Dispatch::Direct`].
pub(crate) fn classify(launch: &Path) -> Dispatch {
    let bin_name = launch.file_name().and_then(OsStr::to_str);
    let target = std::fs::symlink_metadata(launch)
        .ok()
        .filter(|link| link.file_type().is_symlink())
        .and_then(|_| launch.canonicalize().ok());
    match (bin_name, target) {
        (Some(name), Some(target))
            if target.file_name().and_then(OsStr::to_str) == Some(MISE_EXECUTABLE_NAME)
                && name != MISE_EXECUTABLE_NAME =>
        {
            Dispatch::Mise {
                manager: target,
                bin_name: name.to_owned(),
            }
        }
        _ => Dispatch::Direct,
    }
}

/// The owner location variables [`LOCATION_VARS`] of one launch environment.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct OwnerLocations(Vec<(String, String)>);

impl OwnerLocations {
    /// Selects the location variables the launched agent will see.
    pub(crate) fn from_launch(base: &BaseEnv, profile_env: &[(String, String)]) -> Self {
        Self(
            LOCATION_VARS
                .iter()
                .filter_map(|name| {
                    effective_variable(base, profile_env, name)
                        .and_then(|value| value.into_string().ok())
                        .map(|value| ((*name).to_owned(), value))
                })
                .collect(),
        )
    }

    fn pairs(&self) -> &[(String, String)] {
        &self.0
    }
}

impl std::fmt::Debug for OwnerLocations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.0.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_tuple("OwnerLocations").field(&names).finish()
    }
}

/// Why a mise lookup did not name a probed executable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DispatchFailure {
    /// `mise which` exited non-zero: no installed release for the shim.
    Unresolved,
    /// Bad reply (not one absolute UTF-8 path), an untrusted or missing
    /// executable, or the reply names another mise shim (no recursion).
    Invalid,
    /// The lookup outran its deadline.
    TimedOut,
    /// The lookup could not be set up, polled or read.
    Unavailable,
}

/// Asks mise which installed executable the shim `bin_name` would run.
///
/// The manager runs in manager mode (`argv[0]` is its own path) under
/// [`HomeScope::Owner`] with the owner's location variables plus the offline
/// and no-install pairs, starting in [`LOOKUP_WORKING_DIRECTORY`], bounded by
/// `timeout`. The reply must be exactly one absolute UTF-8 path to a trusted,
/// non-shim executable; the trust check is the same rule a launch lookup
/// consults, so the probe and the launch agree. Returns the trust-checked
/// path.
pub(crate) fn resolve(
    manager: &Path,
    bin_name: &str,
    locations: &OwnerLocations,
    launch_path: Option<&OsStr>,
    timeout: Duration,
    now: &impl Fn() -> Instant,
) -> Result<PathBuf, DispatchFailure> {
    let mut env: Vec<(String, String)> = locations.pairs().to_vec();
    env.push((OFFLINE_ENV.0.to_owned(), OFFLINE_ENV.1.to_owned()));
    env.push((
        NO_AUTO_INSTALL_ENV.0.to_owned(),
        NO_AUTO_INSTALL_ENV.1.to_owned(),
    ));
    let args = vec![MISE_WHICH_ARG.to_owned(), bin_name.to_owned()];
    let spec = RunSpec {
        program: manager,
        args: &args,
        search_path: launch_path,
        env: &env,
        home: HomeScope::Owner,
        seeded_env: &[],
        limits: RunLimits::new(timeout, WHICH_OUTPUT_LIMIT),
        stderr: StderrPolicy::Discard,
        stdin: None,
        cwd: Some(Path::new(LOOKUP_WORKING_DIRECTORY)),
    };
    let output = match bounded_run::run(&spec, now, None) {
        RunOutcome::Exited(output) if output.status().success() => output.stdout_lossy(),
        RunOutcome::Exited(_) => return Err(DispatchFailure::Unresolved),
        RunOutcome::TimedOut => return Err(DispatchFailure::TimedOut),
        RunOutcome::Overflow | RunOutcome::Cancelled | RunOutcome::Unavailable => {
            return Err(DispatchFailure::Unavailable)
        }
    };
    let path = parse_which_output(&output).ok_or(DispatchFailure::Invalid)?;
    // The same trust rule a launch lookup applies, so the probe and the
    // launch agree on the executable. An absolute path needs no `PATH`.
    let resolved = shell_env::resolve_executable_in_path_value(path.as_os_str(), None)
        .map_err(|_error| DispatchFailure::Invalid)?;
    // No recursion: a lookup that names another mise shim is refused.
    if matches!(classify(&resolved), Dispatch::Mise { .. }) {
        return Err(DispatchFailure::Invalid);
    }
    Ok(resolved)
}

/// Trims one trailing newline and demands exactly one absolute UTF-8 path.
fn parse_which_output(output: &str) -> Option<PathBuf> {
    let line = output.strip_suffix('\n').unwrap_or(output);
    if line.contains('\n') || line.contains('\r') {
        return None;
    }
    let path = PathBuf::from(line);
    (path.is_absolute() && path.to_str().is_some()).then_some(path)
}
