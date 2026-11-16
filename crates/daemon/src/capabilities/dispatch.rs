//! Mise shim resolution for version probes.
//!
//! A version-manager shim (a symlink to the `mise` executable) dispatches on
//! its own name: the probe must not run the manager's own `--version`.
//! Instead, `mise which <name>` names the installed executable the shim would
//! run, read-only (`MISE_OFFLINE=1`, `MISE_AUTO_INSTALL=0`) and with only the
//! owner's location variables forwarded. The returned executable is probed in
//! the unchanged private sandbox; the launch still execs the shim link, so
//! the manager and any account dispatch on top of it behave as in the user's
//! shell.

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
/// and no-install pairs, in the sandbox cwd, bounded by `timeout`. The reply
/// must be exactly one absolute UTF-8 path to a trusted, non-shim
/// executable; the trust check is [`resolve_executable_for_lookup`], the
/// same rule a lookup consults, so the probe and the launch agree.
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
    Ok(path)
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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::runtime::environment::{base_environment, EnvironmentSource};
    use pohunek_test_support::tempdir;
    use pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST;

    fn write_executable(path: &Path, body: &str) {
        std::fs::write(path, body).expect("write executable");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("make executable");
    }

    /// A fake `mise` executable: `argv[0]` basename `mise` answers `which`,
    /// any other basename runs the install file the lookup would name.
    fn fake_mise(dir: &Path) -> PathBuf {
        let mise = dir.join("mise");
        write_executable(
            &mise,
            "#!/bin/sh\n\
             name=${0##*/}\n\
             if [ \"$name\" = mise ]; then\n\
               [ \"$1\" = which ] || { echo \"mise 2026.11.0 fake\"; exit 0; }\n\
               target=\"$HOME/.local/share/fakemise/installs/$2\"\n\
               [ -f \"$target\" ] || { echo \"no such tool\" >&2; exit 1; }\n\
               cat \"$target\"\n\
             else\n\
               exec \"$(cat \"$HOME/.local/share/fakemise/installs/$name\")\" \" $@\"\n\
             fi\n",
        );
        mise
    }

    fn owner(dir: &Path) -> OwnerLocations {
        let base = base_environment(DEFAULT_ENVIRONMENT_ALLOWLIST, &EnvironmentSource::Process)
            .expect("the default allowlist builds");
        OwnerLocations::from_launch(&base, &[("HOME".to_owned(), dir.display().to_string())])
    }

    #[test]
    fn classify_sorts_a_mise_shim_from_plain_files_and_other_links() {
        let dir = tempdir().expect("private test directory");
        let root = dir.path();
        let mise = fake_mise(root);
        let shim = root.join("codex");
        symlink(&mise, &shim).expect("link shim");
        let plain = root.join("plain");
        write_executable(&plain, "#!/bin/sh\n");
        let other_target = root.join("other-target");
        write_executable(&other_target, "#!/bin/sh\n");
        let other = root.join("other-link");
        symlink(&other_target, &other).expect("link other");
        // A link that is itself named `mise` is the manager, not a shim.
        let subdir = root.join("sub");
        std::fs::create_dir(&subdir).expect("create subdir");
        let mise_link = subdir.join("mise");
        symlink(&mise, &mise_link).expect("link manager under its own name");

        let mise_case = classify(&shim);
        assert_eq!(
            mise_case,
            Dispatch::Mise {
                manager: mise.canonicalize().expect("canonical"),
                bin_name: "codex".to_owned(),
            }
        );
        assert_eq!(classify(&plain), Dispatch::Direct);
        assert_eq!(classify(&other), Dispatch::Direct);
        assert_eq!(classify(&mise_link), Dispatch::Direct);
        assert_eq!(classify(&root.join("missing")), Dispatch::Direct);
    }

    #[test]
    fn which_output_must_be_one_absolute_path() {
        assert_eq!(
            parse_which_output("/opt/tool/bin/agent\n"),
            Some(PathBuf::from("/opt/tool/bin/agent"))
        );
        for bad in [
            "relative/path\n",
            "/two\n/lines\n",
            "/trailing\r\n",
            "",
            "/first\n/second",
        ] {
            assert_eq!(parse_which_output(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn owner_locations_forward_only_the_declared_names() {
        let dir = tempdir().expect("private test directory");
        let root = dir.path();
        let locations = owner(root);
        let names: Vec<&str> = locations
            .pairs()
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert!(names.contains(&"HOME"));
        assert!(
            names.iter().all(|name| LOCATION_VARS.contains(name)),
            "{names:?}"
        );
        let home = locations
            .pairs()
            .iter()
            .find(|(name, _)| name == "HOME")
            .expect("HOME forwarded");
        assert_eq!(home.1, root.display().to_string());
        let debug = format!("{locations:?}");
        assert!(debug.contains("HOME"), "{debug}");
        assert!(!debug.contains(&root.display().to_string()), "{debug}");
    }

    #[test]
    fn resolve_names_the_installed_tool_and_refuses_an_empty_set() {
        let dir = tempdir().expect("private test directory");
        let root = dir.path();
        let mise = fake_mise(root);
        let installs = root.join(".local/share/fakemise/installs");
        std::fs::create_dir_all(&installs).expect("create installs");
        let tool = root.join("tool");
        write_executable(&tool, "#!/bin/sh\necho 1.0.2\n");
        std::fs::write(installs.join("codex"), tool.display().to_string())
            .expect("write install file");
        let locations = owner(root);
        let resolved = resolve(
            &mise,
            "codex",
            &locations,
            None,
            Duration::from_secs(10),
            &Instant::now,
        )
        .expect("installed tool resolves");
        assert_eq!(resolved, tool);

        let missing = resolve(
            &mise,
            "claude",
            &locations,
            None,
            Duration::from_secs(10),
            &Instant::now,
        );
        assert_eq!(missing, Err(DispatchFailure::Unresolved));
    }

    #[test]
    fn resolve_refuses_a_tool_that_is_a_second_shim() {
        let dir = tempdir().expect("private test directory");
        let root = dir.path();
        let mise = fake_mise(root);
        let installs = root.join(".local/share/fakemise/installs");
        std::fs::create_dir_all(&installs).expect("create installs");
        // The lookup names another mise shim: no recursion.
        let inner = root.join("inner");
        symlink(&mise, &inner).expect("link inner shim");
        std::fs::write(installs.join("codex"), inner.display().to_string())
            .expect("write install file");
        let locations = owner(root);
        let refused = resolve(
            &mise,
            "codex",
            &locations,
            None,
            Duration::from_secs(10),
            &Instant::now,
        );
        assert_eq!(refused, Err(DispatchFailure::Invalid));
    }
}
