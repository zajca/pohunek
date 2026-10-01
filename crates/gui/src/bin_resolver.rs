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
    resolve_executable, resolve_search_path, ExecutableError, LoginShellSpec, PathPolicy,
    ResolveError, SearchPath, DARWIN_FALLBACK_DIRECTORIES, PRINTENV_EXECUTABLE,
};
use thiserror::Error;

/// Environment variables a login shell needs to find its startup files.
const LOGIN_SHELL_ENVIRONMENT: [&str; 3] = ["HOME", "USER", "LOGNAME"];

/// Tunables of the login-shell probe.
#[derive(Debug, Clone)]
pub(crate) struct LoginShellSettings {
    pub(crate) timeout: Duration,
    pub(crate) max_output_bytes: usize,
    /// Used when `$SHELL` is unset or not absolute.
    pub(crate) default_shell: PathBuf,
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
        let spec = login_shell_spec(login_shell);
        return discover_search_path(None, Some(&spec), home.as_deref(), None);
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

fn login_shell_spec(settings: &LoginShellSettings) -> LoginShellSpec {
    let shell = std::env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|shell| shell.is_absolute())
        .unwrap_or_else(|| settings.default_shell.clone());
    let environment = LOGIN_SHELL_ENVIRONMENT
        .into_iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| (name.to_owned(), value))
        })
        .collect();
    LoginShellSpec {
        shell,
        printenv: PathBuf::from(PRINTENV_EXECUTABLE),
        environment,
        timeout: settings.timeout,
        max_output_bytes: settings.max_output_bytes,
    }
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
                source: ExecutableError::NotExecutable,
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
}
