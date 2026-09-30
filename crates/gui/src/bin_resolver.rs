//! Resolution of the configured `pohunek_bin` to an absolute executable.
//!
//! An app started from Finder or launchd carries a minimal `PATH`, so a bare
//! `pohunek` would not resolve. A configured absolute path is used as is; a
//! bare name is looked up in a search path resolved by the shared
//! `pohunek_platform::shell_env` policy (login-shell discovery on macOS, the
//! process `PATH` elsewhere, then the documented fallback directories).
//!
//! The resolved search path is cached. Each lookup re-checks the executable on
//! disk, and a miss discards the cache once and discovers again, so an
//! installation done after the GUI started is found without a restart while
//! the bounded login-shell probe runs at most once per miss.

// Rust guideline compliant 2026-09-30
#![forbid(unsafe_code)]

use std::ffi::OsString;
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

/// Reports why `pohunek_bin` did not resolve.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(crate) enum BinError {
    #[error("executable `{program}` is unusable: {source}")]
    Executable {
        program: String,
        #[source]
        source: ExecutableError,
    },
    #[error("cannot determine an executable search path: {0}")]
    SearchPath(String),
}

type Discover = dyn Fn() -> Result<SearchPath, BinError> + Send + Sync;

/// Resolves one configured program name to an absolute executable.
pub(crate) struct BinResolver {
    program: OsString,
    discover: Box<Discover>,
    cache: Mutex<Option<SearchPath>>,
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
    pub(crate) fn for_host(program: &str, login_shell: LoginShellSettings) -> Self {
        Self::with_discovery(program, move || discover_host_search_path(&login_shell))
    }

    /// Builds a resolver around an explicit discovery function.
    pub(crate) fn with_discovery(
        program: &str,
        discover: impl Fn() -> Result<SearchPath, BinError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            program: OsString::from(program),
            discover: Box::new(discover),
            cache: Mutex::new(None),
        }
    }

    /// Returns the absolute path of the executable.
    ///
    /// Blocks while a search path is discovered, so callers run it off the UI
    /// thread.
    ///
    /// # Errors
    ///
    /// Returns [`BinError`] when the name is invalid, a configured path is not
    /// an executable file, no search path can be built, or the name is found
    /// nowhere even after a fresh discovery.
    pub(crate) fn resolve(&self) -> Result<PathBuf, BinError> {
        if self.program.as_encoded_bytes().contains(&b'/') {
            return self.lookup(&SearchPath::empty());
        }
        let cached = self.cache.lock().expect("bin resolver cache lock").clone();
        if let Some(search) = cached {
            match self.lookup(&search) {
                Err(BinError::Executable {
                    source: ExecutableError::NotFound,
                    ..
                }) => {}
                other => return other,
            }
        }
        let fresh = (self.discover)()?;
        let result = self.lookup(&fresh);
        *self.cache.lock().expect("bin resolver cache lock") = Some(fresh);
        result
    }

    fn lookup(&self, search: &SearchPath) -> Result<PathBuf, BinError> {
        resolve_executable(&self.program, search).map_err(|source| BinError::Executable {
            program: self.program.to_string_lossy().into_owned(),
            source,
        })
    }
}

/// Discovers the search path by the documented tier order.
///
/// macOS ignores the inherited `PATH` (a Finder launch carries only the system
/// directories) and asks the login shell; every other host trusts its
/// explicitly inherited `PATH`. Both fall back to the fixed directory list.
fn discover_host_search_path(login_shell: &LoginShellSettings) -> Result<SearchPath, BinError> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let inherited = std::env::var("PATH")
        .ok()
        .and_then(|value| SearchPath::sanitize(&value, true).ok())
        .map(|sanitized| sanitized.path);
    if cfg!(target_os = "macos") {
        let spec = login_shell_spec(login_shell);
        discover_search_path(None, Some(&spec), home.as_deref())
    } else {
        discover_search_path(inherited.as_ref(), None, home.as_deref())
    }
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

fn discover_search_path(
    configured: Option<&SearchPath>,
    login_shell: Option<&LoginShellSpec>,
    home: Option<&Path>,
) -> Result<SearchPath, BinError> {
    let resolution = resolve_search_path(&PathPolicy {
        configured,
        login_shell,
        fallback_directories: DARWIN_FALLBACK_DIRECTORIES,
        home,
    })
    .map_err(|error: ResolveError| BinError::SearchPath(error.to_string()))?;
    if let Some(failure) = &resolution.login_shell_failure {
        // The GUI has no log sink; stderr is the only channel a failed probe
        // reaches. The error text never contains paths or values.
        eprintln!("pohunek-gui: login shell PATH discovery failed ({failure}); using the fallback directories");
    }
    Ok(resolution.path)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;

    fn executable(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\n").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    fn search(dir: &Path) -> SearchPath {
        SearchPath::new(vec![dir.to_path_buf()]).expect("search path")
    }

    #[test]
    fn a_configured_absolute_path_is_used_without_discovery() {
        let dir = tempfile::tempdir().expect("tempdir");
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
        let dir = tempfile::tempdir().expect("tempdir");
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
        let dir = tempfile::tempdir().expect("tempdir");
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
        let stale = tempfile::tempdir().expect("stale");
        let fresh = tempfile::tempdir().expect("fresh");
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
        let first = tempfile::tempdir().expect("first");
        let second = tempfile::tempdir().expect("second");
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
        let dir = tempfile::tempdir().expect("tempdir");
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
        let dir = tempfile::tempdir().expect("tempdir");
        let configured = search(dir.path());

        let resolved = discover_search_path(Some(&configured), None, None).expect("configured");
        assert_eq!(resolved, configured);

        let home = tempfile::tempdir().expect("home");
        let local_bin = home.path().join(".local/bin");
        fs::create_dir_all(&local_bin).expect("mkdir");
        let fallback = discover_search_path(None, None, Some(home.path())).expect("fallback");
        assert!(fallback.entries().contains(&local_bin));
    }
}
