//! Resolution of the programs the GUI starts to an absolute executable.
//!
//! An app started from Finder or launchd carries a minimal `PATH`, so a bare
//! `pohunek`, `kitty`, or `notify-send` would not resolve. A configured
//! absolute path is used as is; a bare name is looked up in a search path
//! resolved by the shared `pohunek_platform::shell_env` policy (login-shell
//! discovery on macOS, the process `PATH` elsewhere, then the documented
//! fallback directories).
//!
//! One resolver serves every program name, so the bounded login-shell probe is
//! shared. The resolved search path is cached; a lookup that misses discards it
//! once and discovers again, so an installation done after the GUI started is
//! found without a restart. Discovery runs under the cache lock, so concurrent
//! callers wait for one probe instead of starting their own.

// Rust guideline compliant 2026-10-01
#![forbid(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use pohunek_platform::shell_env::{
    login_environment, resolve_executable, resolve_search_path, ExecutableError,
    LoginEnvironmentError, LoginShellSpec, PathPolicy, ResolveError, SearchPath,
    DARWIN_FALLBACK_DIRECTORIES, PRINTENV_EXECUTABLE,
};
use thiserror::Error;

/// Tunables of the login-shell probe.
#[derive(Debug, Clone)]
pub(crate) struct LoginShellSettings {
    pub(crate) timeout: Duration,
    pub(crate) max_output_bytes: usize,
}

/// Reports why a program did not resolve.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(crate) enum BinError {
    /// The program is unusable. `cause` is why the search path is what it is
    /// (a failed login-shell probe, an unusable inherited `PATH`), when that
    /// explains a miss.
    #[error("executable `{program}` is unusable: {source}{}", cause_suffix(.cause.as_deref()))]
    Executable {
        program: String,
        #[source]
        source: ExecutableError,
        cause: Option<String>,
    },
    #[error("cannot determine an executable search path: {0}")]
    SearchPath(String),
}

fn cause_suffix(cause: Option<&str>) -> String {
    cause.map_or_else(String::new, |cause| format!(" ({cause})"))
}

/// A discovered search path and the reason it fell back, when it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Discovery {
    pub(crate) path: SearchPath,
    /// Why a higher tier was skipped; shown when a lookup misses.
    pub(crate) cause: Option<String>,
}

type Discover = dyn Fn() -> Result<Discovery, BinError> + Send + Sync;

/// Resolves program names to absolute executables.
pub(crate) struct BinResolver {
    program: OsString,
    discover: Box<Discover>,
    cache: Mutex<Option<Discovery>>,
}

impl fmt::Debug for BinResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BinResolver")
            .field("program", &self.program)
            .finish_non_exhaustive()
    }
}

impl BinResolver {
    /// Builds the resolver that discovers the search path of this host.
    ///
    /// `program` is the name [`Self::resolve`] returns; other names go through
    /// [`Self::resolve_name`].
    pub(crate) fn for_host(program: &str, login_shell: LoginShellSettings) -> Self {
        Self::with_discovery_cause(program, move || discover_host_search_path(&login_shell))
    }

    /// Builds a resolver around an explicit discovery function.
    #[cfg(test)]
    pub(crate) fn with_discovery(
        program: &str,
        discover: impl Fn() -> Result<SearchPath, BinError> + Send + Sync + 'static,
    ) -> Self {
        Self::with_discovery_cause(program, move || {
            discover().map(|path| Discovery { path, cause: None })
        })
    }

    /// Builds a resolver around a discovery function that reports its cause.
    pub(crate) fn with_discovery_cause(
        program: &str,
        discover: impl Fn() -> Result<Discovery, BinError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            program: OsString::from(program),
            discover: Box::new(discover),
            cache: Mutex::new(None),
        }
    }

    /// Returns the absolute path of the primary program.
    ///
    /// Blocks while a search path is discovered, so callers run it off the UI
    /// thread.
    ///
    /// # Errors
    ///
    /// See [`Self::resolve_name`].
    pub(crate) fn resolve(&self) -> Result<PathBuf, BinError> {
        self.resolve_name(&self.program)
    }

    /// Returns the absolute path of `name`.
    ///
    /// A name containing `/` must be absolute and executable; a bare name is
    /// searched.
    ///
    /// # Errors
    ///
    /// Returns [`BinError`] when the name is invalid, a configured path is not
    /// an executable file, no search path can be built, or the name is found
    /// nowhere even after a fresh discovery.
    pub(crate) fn resolve_name(&self, name: &OsStr) -> Result<PathBuf, BinError> {
        if name.as_encoded_bytes().contains(&b'/') {
            return lookup(
                name,
                &Discovery {
                    path: SearchPath::empty(),
                    cause: None,
                },
            );
        }
        let mut cache = self.cache.lock().expect("bin resolver cache lock");
        if let Some(discovery) = cache.as_ref() {
            match lookup(name, discovery) {
                Err(BinError::Executable {
                    source: ExecutableError::NotFound,
                    ..
                }) => {}
                other => return other,
            }
        }
        let fresh = (self.discover)()?;
        let result = lookup(name, &fresh);
        *cache = Some(fresh);
        result
    }
}

fn lookup(name: &OsStr, discovery: &Discovery) -> Result<PathBuf, BinError> {
    resolve_executable(name, &discovery.path).map_err(|source| BinError::Executable {
        program: name.to_string_lossy().into_owned(),
        cause: matches!(source, ExecutableError::NotFound)
            .then(|| discovery.cause.clone())
            .flatten(),
        source,
    })
}

/// Discovers the search path by the documented tier order.
///
/// macOS ignores the inherited `PATH` (a Finder launch carries only the system
/// directories) and asks the login shell; every other host trusts its
/// explicitly inherited `PATH`. Both fall back to the fixed directory list, and
/// the reason for a fallback is carried in [`Discovery::cause`].
fn discover_host_search_path(login_shell: &LoginShellSettings) -> Result<Discovery, BinError> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if cfg!(target_os = "macos") {
        return match login_shell_spec(login_shell, |name| std::env::var_os(name)) {
            Ok(spec) => discover_search_path(None, Some(&spec), home.as_deref(), None),
            // An unusable `$SHELL` or profile selector cannot start the probe;
            // the fallback directories apply and the cause is shown on a miss.
            Err(error) => discover_search_path(
                None,
                None,
                home.as_deref(),
                Some(format!(
                    "the login shell environment is unusable: {error}; searched the fallback directories"
                )),
            ),
        };
    }
    let inherited_path = std::env::var("PATH")
        .map_err(|error| error.to_string())
        .and_then(|value| {
            SearchPath::sanitize(&value, true)
                .map(|sanitized| sanitized.path)
                .map_err(|error| error.to_string())
        });
    let (inherited, inherited_cause) = match inherited_path {
        Ok(path) => (Some(path), None),
        Err(reason) => (
            None,
            Some(format!("the inherited PATH is unusable: {reason}")),
        ),
    };
    discover_search_path(inherited.as_ref(), None, home.as_deref(), inherited_cause)
}

/// Builds the login-shell probe from the process environment.
///
/// The environment (identity variables, `ZDOTDIR`, `XDG_CONFIG_HOME`, and
/// `$SHELL`) and its validation are the shared
/// [`pohunek_platform::shell_env::login_environment`], the same one the service
/// installer uses. `lookup` is injected for tests.
fn login_shell_spec(
    settings: &LoginShellSettings,
    lookup: impl Fn(&str) -> Option<OsString>,
) -> Result<LoginShellSpec, LoginEnvironmentError> {
    let login = login_environment(lookup)?;
    Ok(LoginShellSpec {
        shell: login.shell,
        printenv: PathBuf::from(PRINTENV_EXECUTABLE),
        environment: login.variables,
        timeout: settings.timeout,
        max_output_bytes: settings.max_output_bytes,
    })
}

/// Resolves the policy tiers; `prior_cause` explains a tier the caller already
/// skipped.
fn discover_search_path(
    configured: Option<&SearchPath>,
    login_shell: Option<&LoginShellSpec>,
    home: Option<&Path>,
    prior_cause: Option<String>,
) -> Result<Discovery, BinError> {
    let resolution = resolve_search_path(&PathPolicy {
        configured,
        login_shell,
        fallback_directories: DARWIN_FALLBACK_DIRECTORIES,
        home,
    })
    .map_err(|error: ResolveError| BinError::SearchPath(error.to_string()))?;
    let cause = resolution
        .login_shell_failure
        .as_ref()
        .map(|failure| {
            format!(
                "login shell PATH discovery failed: {failure}; searched the fallback directories"
            )
        })
        .or(prior_cause);
    Ok(Discovery {
        path: resolution.path,
        cause,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;

    fn executable(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\n").expect("write");
        crate::test_support::make_executable(&path);
        path
    }

    fn search(dir: &Path) -> SearchPath {
        SearchPath::new(vec![dir.to_path_buf()]).expect("search path")
    }

    #[test]
    fn a_configured_absolute_path_is_used_without_discovery() {
        let dir = crate::test_support::fixture();
        let bin = executable(dir.path(), "pohunek");
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let resolver = BinResolver::with_discovery(bin.to_str().expect("utf8"), move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Err(BinError::SearchPath("must not run".to_owned()))
        });

        assert_eq!(resolver.resolve().expect("resolve"), bin);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_configured_path_that_is_not_executable_is_an_error_not_a_search() {
        let dir = crate::test_support::fixture();
        let path = dir.path().join("pohunek");
        fs::write(&path, "data").expect("write");
        let resolver = BinResolver::with_discovery(path.to_str().expect("utf8"), || {
            Err(BinError::SearchPath("must not run".to_owned()))
        });

        assert!(matches!(
            resolver.resolve(),
            Err(BinError::Executable {
                source: ExecutableError::NotExecutable(_),
                ..
            })
        ));
    }

    #[test]
    fn a_relative_path_is_rejected() {
        let resolver = BinResolver::with_discovery("bin/pohunek", || {
            Err(BinError::SearchPath("must not run".to_owned()))
        });

        assert!(matches!(
            resolver.resolve(),
            Err(BinError::Executable {
                source: ExecutableError::RelativePath,
                ..
            })
        ));
    }

    #[test]
    fn a_bare_name_resolves_through_the_discovered_search_path_once() {
        let dir = crate::test_support::fixture();
        let bin = executable(dir.path(), "pohunek");
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let search_path = search(dir.path());
        let resolver = BinResolver::with_discovery("pohunek", move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(search_path.clone())
        });

        assert_eq!(resolver.resolve().expect("first"), bin);
        assert_eq!(resolver.resolve().expect("second"), bin);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "discovery result is cached"
        );
    }

    #[test]
    fn a_miss_discards_the_cache_and_finds_a_later_installation() {
        let stale = crate::test_support::fixture();
        let fresh = crate::test_support::fixture();
        let bin = executable(fresh.path(), "pohunek");
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let stale_search = search(stale.path());
        let fresh_search = search(fresh.path());
        let resolver = BinResolver::with_discovery("pohunek", move || {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(stale_search.clone())
            } else {
                Ok(fresh_search.clone())
            }
        });

        // The first discovery yields a directory without the binary.
        assert!(matches!(
            resolver.resolve(),
            Err(BinError::Executable {
                source: ExecutableError::NotFound,
                ..
            })
        ));
        // The next attach re-discovers and finds it.
        assert_eq!(resolver.resolve().expect("after miss"), bin);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(resolver.resolve().expect("cached"), bin);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_cached_binary_that_vanished_is_rediscovered_once() {
        let first = crate::test_support::fixture();
        let second = crate::test_support::fixture();
        let first_bin = executable(first.path(), "pohunek");
        let second_bin = executable(second.path(), "pohunek");
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let first_search = search(first.path());
        let second_search = search(second.path());
        let resolver = BinResolver::with_discovery("pohunek", move || {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(first_search.clone())
            } else {
                Ok(second_search.clone())
            }
        });
        assert_eq!(resolver.resolve().expect("first"), first_bin);
        fs::remove_file(&first_bin).expect("remove");

        assert_eq!(resolver.resolve().expect("second"), second_bin);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_missing_name_after_fresh_discovery_is_a_clear_error() {
        let dir = crate::test_support::fixture();
        let search_path = search(dir.path());
        let resolver = BinResolver::with_discovery("pohunek", move || Ok(search_path.clone()));

        let error = resolver.resolve().expect_err("not installed");

        assert!(matches!(
            error,
            BinError::Executable {
                source: ExecutableError::NotFound,
                ..
            }
        ));
        assert!(error.to_string().contains("`pohunek`"), "{error}");
    }

    #[test]
    fn a_discovery_failure_is_reported_not_replaced_by_a_default() {
        let resolver = BinResolver::with_discovery("pohunek", || {
            Err(BinError::SearchPath("no directory".to_owned()))
        });

        assert_eq!(
            resolver.resolve(),
            Err(BinError::SearchPath("no directory".to_owned()))
        );
    }

    #[test]
    fn discovery_prefers_the_configured_path_and_falls_back_to_the_table() {
        let dir = crate::test_support::fixture();
        let configured = search(dir.path());

        let resolved =
            discover_search_path(Some(&configured), None, None, None).expect("configured");
        assert_eq!(resolved.path, configured);

        let home = crate::test_support::fixture();
        let local_bin = home.path().join(".local/bin");
        fs::create_dir_all(&local_bin).expect("mkdir");
        let fallback = discover_search_path(None, None, Some(home.path()), None).expect("fallback");
        assert!(fallback.path.entries().contains(&local_bin));
    }

    #[test]
    fn a_miss_carries_the_reason_the_search_path_fell_back() {
        let dir = crate::test_support::fixture();
        let path = search(dir.path());
        let resolver = BinResolver::with_discovery_cause("pohunek", move || {
            Ok(Discovery {
                path: path.clone(),
                cause: Some("login shell PATH discovery failed: timed out".to_owned()),
            })
        });

        let error = resolver.resolve().expect_err("not installed");

        assert!(
            error
                .to_string()
                .contains("login shell PATH discovery failed: timed out"),
            "{error}"
        );
    }

    #[test]
    fn a_hit_does_not_report_the_fallback_cause() {
        let dir = crate::test_support::fixture();
        executable(dir.path(), "pohunek");
        let path = search(dir.path());
        let resolver = BinResolver::with_discovery_cause("pohunek", move || {
            Ok(Discovery {
                path: path.clone(),
                cause: Some("ignored".to_owned()),
            })
        });

        resolver.resolve().expect("found");
    }

    #[test]
    fn other_names_share_the_cached_search_path() {
        let dir = crate::test_support::fixture();
        let kitty = executable(dir.path(), "kitty");
        executable(dir.path(), "pohunek");
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let path = search(dir.path());
        let resolver = BinResolver::with_discovery("pohunek", move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(path.clone())
        });

        resolver.resolve().expect("pohunek");
        assert_eq!(
            resolver.resolve_name(OsStr::new("kitty")).expect("kitty"),
            kitty
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn concurrent_callers_share_one_discovery() {
        let dir = crate::test_support::fixture();
        executable(dir.path(), "pohunek");
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let path = search(dir.path());
        let resolver = Arc::new(BinResolver::with_discovery("pohunek", move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(path.clone())
        }));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let resolver = Arc::clone(&resolver);
                std::thread::spawn(move || resolver.resolve().expect("resolve"))
            })
            .collect();
        for handle in handles {
            handle.join().expect("thread");
        }

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    fn settings() -> LoginShellSettings {
        LoginShellSettings {
            timeout: Duration::from_secs(10),
            max_output_bytes: 4096,
        }
    }

    fn lookup(pairs: Vec<(&'static str, OsString)>) -> impl Fn(&str) -> Option<OsString> {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        }
    }

    /// Writes a fake login shell: `profile` runs first (it may set `PATH` from
    /// the variables the probe passed), then the probe script (`$3`) runs.
    fn fake_shell(dir: &Path, profile: &str) -> PathBuf {
        let path = dir.join("fake-shell");
        fs::write(
            &path,
            format!("#!/bin/sh\n{profile}\nexport PATH\nexec /bin/sh -c \"$3\"\n"),
        )
        .expect("write shell");
        crate::test_support::make_executable(&path);
        path
    }

    fn probe(shell: &Path, extra: Vec<(&'static str, OsString)>) -> SearchPath {
        let mut pairs = vec![("SHELL", OsString::from(shell.as_os_str()))];
        pairs.extend(extra);
        let spec = login_shell_spec(&settings(), lookup(pairs)).expect("spec");
        discover_search_path(None, Some(&spec), None, None)
            .expect("discovery")
            .path
    }

    #[test]
    fn a_profile_that_branches_on_shell_reaches_the_probe() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let prefix = dir.path().join("by-shell/bin");
        fs::create_dir_all(&prefix).expect("prefix");
        // The profile sets its PATH only when $SHELL names this very shell.
        let shell = fake_shell(
            dir.path(),
            &format!("[ \"$SHELL\" = \"$0\" ] && PATH='{}'", prefix.display()),
        );

        // The trusted fallback directories follow the discovered ones.
        assert_eq!(probe(&shell, vec![]).entries()[0], prefix);
    }

    #[test]
    fn zdotdir_and_xdg_config_home_reach_the_probe() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let zsh_prefix = dir.path().join("zdot-prefix");
        let fish_prefix = dir.path().join("xdg-prefix");
        fs::create_dir_all(&zsh_prefix).expect("zsh prefix");
        fs::create_dir_all(&fish_prefix).expect("fish prefix");
        let zdotdir = dir.path().join("zdotdir");
        let xdg = dir.path().join("xdg");
        fs::create_dir_all(&zdotdir).expect("zdotdir");
        fs::create_dir_all(&xdg).expect("xdg");
        fs::write(zdotdir.join("path"), zsh_prefix.display().to_string()).expect("zsh profile");
        fs::write(xdg.join("path"), fish_prefix.display().to_string()).expect("xdg profile");
        let shell = fake_shell(
            dir.path(),
            "[ -n \"$ZDOTDIR\" ] && PATH=\"$(/bin/cat \"$ZDOTDIR/path\")\"\n[ -n \"$XDG_CONFIG_HOME\" ] && PATH=\"$PATH:$(/bin/cat \"$XDG_CONFIG_HOME/path\")\"",
        );

        let only_zsh = probe(
            &shell,
            vec![("ZDOTDIR", OsString::from(zdotdir.as_os_str()))],
        );
        assert_eq!(only_zsh.entries()[0], zsh_prefix);

        let both = probe(
            &shell,
            vec![
                ("ZDOTDIR", OsString::from(zdotdir.as_os_str())),
                ("XDG_CONFIG_HOME", OsString::from(xdg.as_os_str())),
            ],
        );
        assert_eq!(both.entries()[..2], [zsh_prefix, fish_prefix]);
    }

    #[test]
    fn an_unusable_shell_environment_is_a_typed_failure() {
        let error = login_shell_spec(&settings(), lookup(vec![("ZDOTDIR", "relative".into())]))
            .expect_err("relative ZDOTDIR");

        assert!(
            matches!(
                error,
                LoginEnvironmentError::NotAbsolute { var: "ZDOTDIR", .. }
            ),
            "{error:?}"
        );
    }
}
