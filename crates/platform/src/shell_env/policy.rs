//! The ordered `PATH` resolution policy.

use std::path::Path;

use thiserror::Error;

use super::login_shell::{discover_login_shell_path, LoginShellError, LoginShellSpec};
use super::search_path::{fallback_search_path, SearchPath, SearchPathError};

/// Inputs of [`resolve_search_path`].
#[derive(Debug, Clone, Copy)]
pub struct PathPolicy<'a> {
    /// An explicitly configured profile or service `PATH`; authoritative when
    /// present and non-empty.
    pub configured: Option<&'a SearchPath>,
    /// Login-shell discovery to attempt, or `None` where it does not apply.
    pub login_shell: Option<&'a LoginShellSpec>,
    /// Fallback directory table, see
    /// [`DARWIN_FALLBACK_DIRECTORIES`](super::DARWIN_FALLBACK_DIRECTORIES).
    pub fallback_directories: &'a [&'a str],
    /// Home directory expanding `~/` fallback entries.
    pub home: Option<&'a Path>,
}

/// Which tier produced a resolved `PATH`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PathSource {
    /// An explicitly configured profile or service `PATH`.
    Configured,
    /// The `PATH` a login shell built.
    LoginShell,
    /// The built-in fallback directory list.
    FallbackDirectories,
}

impl PathSource {
    /// Returns the stable lowercase name used in logs and diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::LoginShell => "login_shell",
            Self::FallbackDirectories => "fallback_directories",
        }
    }
}

/// A resolved `PATH` and how it was obtained.
#[derive(Debug)]
pub struct PathResolution {
    /// The directories to search.
    pub path: SearchPath,
    /// The tier that produced [`path`](Self::path).
    pub source: PathSource,
    /// Why login-shell discovery was attempted and failed, when the fallback
    /// list was used because of it; `None` when discovery was not attempted.
    pub login_shell_failure: Option<LoginShellError>,
    /// Entries of the login shell's value dropped during sanitization.
    pub dropped: usize,
}

/// Reports that no tier produced a usable `PATH`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ResolveError {
    /// The fallback list holds no existing directory, so nothing can run.
    #[error("no configured, discovered, or fallback directory is usable")]
    NoUsableDirectories(#[source] SearchPathError),
}

/// Resolves the executable search path by the documented tier order.
///
/// A non-empty `configured` path wins without running anything. Otherwise
/// the login shell is probed when `login_shell` is set; a probe failure never
/// aborts and never invents a value: the fallback directory list is used and
/// the failure is returned in [`PathResolution::login_shell_failure`].
///
/// # Errors
///
/// Returns [`ResolveError::NoUsableDirectories`] when the fallback list has no
/// existing directory either.
pub fn resolve_search_path(policy: &PathPolicy<'_>) -> Result<PathResolution, ResolveError> {
    if let Some(configured) = policy.configured.filter(|path| !path.is_empty()) {
        return Ok(PathResolution {
            path: configured.clone(),
            source: PathSource::Configured,
            login_shell_failure: None,
            dropped: 0,
        });
    }
    let mut failure = None;
    if let Some(spec) = policy.login_shell {
        match discover_login_shell_path(spec) {
            Ok(discovery) => {
                return Ok(PathResolution {
                    path: discovery.path,
                    source: PathSource::LoginShell,
                    login_shell_failure: None,
                    dropped: discovery.dropped,
                });
            }
            Err(error) => failure = Some(error),
        }
    }
    let path = fallback_search_path(policy.fallback_directories, policy.home)
        .map_err(ResolveError::NoUsableDirectories)?;
    Ok(PathResolution {
        path,
        source: PathSource::FallbackDirectories,
        login_shell_failure: failure,
        dropped: 0,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::PathBuf;
    use std::time::Duration;

    use super::super::login_shell::PRINTENV_EXECUTABLE;
    use super::*;

    fn shell(dir: &Path, body: &str) -> LoginShellSpec {
        let path = dir.join("shell");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        LoginShellSpec {
            shell: path,
            printenv: PathBuf::from(PRINTENV_EXECUTABLE),
            environment: Vec::new(),
            timeout: Duration::from_secs(10),
            max_output_bytes: 4096,
        }
    }

    /// Lays out a fake home with Apple Silicon Homebrew and user installs.
    fn fake_host(root: &Path) -> (PathBuf, Vec<String>, Vec<PathBuf>) {
        let home = root.join("Users/me");
        let brew = root.join("opt/homebrew/bin");
        let local = home.join(".local/bin");
        let cargo = home.join(".cargo/bin");
        for dir in [&brew, &local, &cargo] {
            fs::create_dir_all(dir).expect("dir");
        }
        let table = vec![
            "~/.local/bin".to_owned(),
            "~/.cargo/bin".to_owned(),
            "~/.bun/bin".to_owned(),
            brew.display().to_string(),
            root.join("usr/local/bin").display().to_string(),
        ];
        (home, table, vec![local, cargo, brew])
    }

    fn refs(table: &[String]) -> Vec<&str> {
        table.iter().map(String::as_str).collect()
    }

    #[test]
    fn a_configured_path_is_authoritative_and_runs_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("ran");
        let spec = shell(dir.path(), &format!("touch '{}'", marker.display()));
        let configured = SearchPath::new(vec![dir.path().to_path_buf()]).expect("configured path");
        let resolution = resolve_search_path(&PathPolicy {
            configured: Some(&configured),
            login_shell: Some(&spec),
            fallback_directories: &[],
            home: None,
        })
        .expect("resolution");
        assert_eq!(resolution.source, PathSource::Configured);
        assert_eq!(resolution.path, configured);
        assert!(!marker.exists(), "no login shell may run");
    }

    #[test]
    fn an_empty_configured_path_does_not_count_as_configured() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (home, table, _) = fake_host(dir.path());
        let empty = SearchPath::empty();
        let resolution = resolve_search_path(&PathPolicy {
            configured: Some(&empty),
            login_shell: None,
            fallback_directories: &refs(&table),
            home: Some(&home),
        })
        .expect("resolution");
        assert_eq!(resolution.source, PathSource::FallbackDirectories);
    }

    #[test]
    fn login_shell_wins_over_the_fallback_list() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (home, table, _) = fake_host(dir.path());
        let discovered = dir.path().join("nix/bin");
        fs::create_dir_all(&discovered).expect("nix");
        let spec = shell(
            dir.path(),
            &format!(
                "PATH='{}'; export PATH\nexec /bin/sh -c \"$3\"",
                discovered.display()
            ),
        );
        let resolution = resolve_search_path(&PathPolicy {
            configured: None,
            login_shell: Some(&spec),
            fallback_directories: &refs(&table),
            home: Some(&home),
        })
        .expect("resolution");
        assert_eq!(resolution.source, PathSource::LoginShell);
        assert_eq!(resolution.path.entries(), [discovered]);
        assert!(resolution.login_shell_failure.is_none());
    }

    #[test]
    fn a_failing_login_shell_falls_back_and_reports_why() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (home, table, expected) = fake_host(dir.path());
        for body in ["exit 1", "echo junk", "exit 0"] {
            let spec = shell(dir.path(), body);
            let resolution = resolve_search_path(&PathPolicy {
                configured: None,
                login_shell: Some(&spec),
                fallback_directories: &refs(&table),
                home: Some(&home),
            })
            .expect("resolution");
            assert_eq!(resolution.source, PathSource::FallbackDirectories, "{body}");
            assert!(resolution.login_shell_failure.is_some(), "{body}");
            assert_eq!(resolution.path.entries(), expected, "{body}");
        }
    }

    #[test]
    fn a_hanging_login_shell_falls_back_after_the_deadline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (home, table, expected) = fake_host(dir.path());
        let mut spec = shell(dir.path(), "exec sleep 300");
        spec.timeout = Duration::from_millis(500);
        let resolution = resolve_search_path(&PathPolicy {
            configured: None,
            login_shell: Some(&spec),
            fallback_directories: &refs(&table),
            home: Some(&home),
        })
        .expect("resolution");
        assert_eq!(resolution.source, PathSource::FallbackDirectories);
        assert!(matches!(
            resolution.login_shell_failure,
            Some(LoginShellError::Timeout { .. })
        ));
        assert_eq!(resolution.path.entries(), expected);
    }

    #[test]
    fn the_fallback_list_covers_apple_silicon_and_user_installs_only_where_they_exist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (home, table, expected) = fake_host(dir.path());
        let resolution = resolve_search_path(&PathPolicy {
            configured: None,
            login_shell: None,
            fallback_directories: &refs(&table),
            home: Some(&home),
        })
        .expect("resolution");
        // `~/.bun/bin` and the Intel prefix do not exist here and are skipped.
        assert_eq!(resolution.path.entries(), expected);
        assert!(resolution.login_shell_failure.is_none());
    }

    #[test]
    fn home_relative_entries_are_skipped_without_a_home() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_home, table, expected) = fake_host(dir.path());
        let resolution = resolve_search_path(&PathPolicy {
            configured: None,
            login_shell: None,
            fallback_directories: &refs(&table),
            home: None,
        })
        .expect("resolution");
        assert_eq!(resolution.path.entries(), &expected[2..]);
    }

    #[test]
    fn nothing_usable_is_an_error_not_an_invented_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("missing").display().to_string();
        let error = resolve_search_path(&PathPolicy {
            configured: None,
            login_shell: None,
            fallback_directories: &[missing.as_str()],
            home: None,
        })
        .expect_err("no directory");
        assert!(matches!(error, ResolveError::NoUsableDirectories(_)));
    }
}
