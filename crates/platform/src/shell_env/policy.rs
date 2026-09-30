//! The ordered `PATH` resolution policy.

use std::path::{Path, PathBuf};

use thiserror::Error;

use super::login_shell::{discover_login_shell_path, LoginShellError, LoginShellSpec};
use super::search_path::{fallback_search_path, DroppedEntry, SearchPath, SearchPathError};

/// Inputs of [`resolve_search_path`].
#[derive(Debug, Clone, Copy)]
pub struct PathPolicy<'a> {
    /// An explicitly supplied environment `PATH` (a GUI launched from a shell
    /// passes its inherited one); authoritative when present and non-empty. The
    /// installer supplies none.
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
    /// An explicitly supplied environment `PATH`.
    Configured,
    /// The `PATH` a login shell built, followed by the fallback directories it
    /// lacked.
    LoginShell,
    /// The built-in fallback directory list alone.
    FallbackDirectories,
}

impl PathSource {
    /// Returns the stable lowercase name used in reports and diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::LoginShell => "login_shell",
            Self::FallbackDirectories => "fallback",
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
    /// The login shell that was tried, when discovery was attempted.
    pub shell: Option<PathBuf>,
    /// Why login-shell discovery was attempted and failed, when the fallback
    /// list was used because of it; `None` when discovery was not attempted.
    pub login_shell_failure: Option<LoginShellError>,
    /// Existing directories refused as untrusted, from every tier consulted.
    pub untrusted: Vec<DroppedEntry>,
    /// Entries ignored without concern (empty, relative, duplicate, missing).
    pub ignored: usize,
}

/// Reports that no tier produced a usable `PATH`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ResolveError {
    /// The fallback list holds no trusted directory, so nothing can run.
    #[error("no discovered or fallback directory is usable")]
    NoUsableDirectories(#[source] SearchPathError),
}

/// Resolves the executable search path by the documented tier order.
///
/// A non-empty `configured` path wins without running anything. Otherwise the
/// login shell is probed when `login_shell` is set. A successful probe may see
/// only login startup files (a non-interactive `-l` shell skips `.zshrc`), so
/// the trusted fallback directories it lacks are appended after the discovered
/// ones. A failed probe never aborts and never invents a value: the fallback
/// list alone is used and the failure is returned in
/// [`PathResolution::login_shell_failure`]. Every tier keeps only trusted
/// directories; refused ones are reported in [`PathResolution::untrusted`].
///
/// # Errors
///
/// Returns [`ResolveError::NoUsableDirectories`] when discovery failed and the
/// fallback list has no trusted directory either.
pub fn resolve_search_path(policy: &PathPolicy<'_>) -> Result<PathResolution, ResolveError> {
    if let Some(configured) = policy.configured.filter(|path| !path.is_empty()) {
        return Ok(PathResolution {
            path: configured.clone(),
            source: PathSource::Configured,
            shell: None,
            login_shell_failure: None,
            untrusted: Vec::new(),
            ignored: 0,
        });
    }
    let fallback = fallback_search_path(policy.fallback_directories, policy.home);
    let shell = policy.login_shell.map(|spec| spec.shell.clone());
    let mut failure = None;
    if let Some(spec) = policy.login_shell {
        match discover_login_shell_path(spec) {
            Ok(discovery) => {
                let mut untrusted = discovery.untrusted;
                let mut ignored = discovery.ignored;
                let path = match fallback {
                    Ok(extra) => {
                        for dropped in extra.untrusted {
                            // One report per directory, even when both lists name it.
                            if !untrusted.iter().any(|known| known.entry == dropped.entry) {
                                untrusted.push(dropped);
                            }
                        }
                        ignored += extra.ignored;
                        discovery.path.with_appended(extra.path.entries())
                    }
                    Err(_no_fallback) => discovery.path,
                };
                return Ok(PathResolution {
                    path,
                    source: PathSource::LoginShell,
                    shell,
                    login_shell_failure: None,
                    untrusted,
                    ignored,
                });
            }
            Err(error) => failure = Some(error),
        }
    }
    let fallback = fallback.map_err(ResolveError::NoUsableDirectories)?;
    Ok(PathResolution {
        path: fallback.path,
        source: PathSource::FallbackDirectories,
        shell,
        login_shell_failure: failure,
        untrusted: fallback.untrusted,
        ignored: fallback.ignored,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use super::super::login_shell::PRINTENV_EXECUTABLE;
    use super::*;
    use crate::shell_env::test_support::{fixture, make_dir, script};

    fn shell(dir: &Path, body: &str) -> LoginShellSpec {
        LoginShellSpec {
            shell: script(dir, "shell", body),
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
            make_dir(root, dir);
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

    fn policy<'a>(
        login_shell: Option<&'a LoginShellSpec>,
        table: &'a [&'a str],
        home: Option<&'a Path>,
    ) -> PathPolicy<'a> {
        PathPolicy {
            configured: None,
            login_shell,
            fallback_directories: table,
            home,
        }
    }

    #[test]
    fn a_supplied_path_is_authoritative_and_runs_nothing() {
        let dir = fixture();
        let marker = dir.path().join("ran");
        let spec = shell(dir.path(), &format!("touch '{}'", marker.display()));
        let configured = SearchPath::new(vec![dir.path().to_path_buf()]).expect("path");
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
    fn an_empty_supplied_path_does_not_count() {
        let dir = fixture();
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
    fn discovered_entries_come_first_and_missing_fallback_directories_are_appended() {
        let dir = fixture();
        let (home, table, fallback) = fake_host(dir.path());
        let discovered = dir.path().join("nix/bin");
        make_dir(dir.path(), &discovered);
        let spec = shell(
            dir.path(),
            &format!(
                "PATH='{}:{}'; export PATH\nexec /bin/sh -c \"$3\"",
                discovered.display(),
                fallback[2].display()
            ),
        );
        let resolution = resolve_search_path(&policy(Some(&spec), &refs(&table), Some(&home)))
            .expect("resolution");
        assert_eq!(resolution.source, PathSource::LoginShell);
        // A login shell skips `.zshrc`, so `~/.local/bin` and `~/.cargo/bin`
        // may be absent from its PATH; they are appended, without duplicates.
        assert_eq!(
            resolution.path.entries(),
            [
                discovered,
                fallback[2].clone(),
                fallback[0].clone(),
                fallback[1].clone()
            ]
        );
        assert!(resolution.login_shell_failure.is_none());
        assert_eq!(resolution.shell.as_deref(), Some(spec.shell.as_path()));
    }

    #[test]
    fn a_failing_login_shell_falls_back_and_reports_why() {
        let dir = fixture();
        let (home, table, expected) = fake_host(dir.path());
        for body in ["exit 1", "echo junk", "exit 0"] {
            let spec = shell(dir.path(), body);
            let resolution = resolve_search_path(&policy(Some(&spec), &refs(&table), Some(&home)))
                .expect("resolution");
            assert_eq!(resolution.source, PathSource::FallbackDirectories, "{body}");
            assert!(resolution.login_shell_failure.is_some(), "{body}");
            assert_eq!(resolution.path.entries(), expected, "{body}");
            assert_eq!(resolution.shell.as_deref(), Some(spec.shell.as_path()));
        }
    }

    #[test]
    fn a_hanging_login_shell_falls_back_after_the_deadline() {
        let dir = fixture();
        let (home, table, expected) = fake_host(dir.path());
        let mut spec = shell(dir.path(), "exec sleep 300");
        spec.timeout = Duration::from_millis(500);
        let resolution = resolve_search_path(&policy(Some(&spec), &refs(&table), Some(&home)))
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
        let dir = fixture();
        let (home, table, expected) = fake_host(dir.path());
        let resolution =
            resolve_search_path(&policy(None, &refs(&table), Some(&home))).expect("resolution");
        // `~/.bun/bin` and the Intel prefix do not exist here and are skipped.
        assert_eq!(resolution.path.entries(), expected);
        assert!(resolution.login_shell_failure.is_none());
        assert!(resolution.shell.is_none());
    }

    #[test]
    fn untrusted_fallback_directories_are_reported_not_used() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = fixture();
        let (home, table, expected) = fake_host(dir.path());
        let loose = dir.path().join("usr/local/bin");
        make_dir(dir.path(), &loose);
        fs::set_permissions(&loose, fs::Permissions::from_mode(0o775)).expect("chmod");
        let resolution =
            resolve_search_path(&policy(None, &refs(&table), Some(&home))).expect("resolution");
        assert_eq!(resolution.path.entries(), expected);
        assert_eq!(resolution.untrusted.len(), 1);
        assert_eq!(resolution.untrusted[0].entry, loose.display().to_string());
        assert_eq!(
            resolution.untrusted[0].reason,
            "writable by group or others"
        );
    }

    #[test]
    fn home_relative_entries_are_skipped_without_a_home() {
        let dir = fixture();
        let (_home, table, expected) = fake_host(dir.path());
        let resolution =
            resolve_search_path(&policy(None, &refs(&table), None)).expect("resolution");
        assert_eq!(resolution.path.entries(), &expected[2..]);
    }

    #[test]
    fn nothing_usable_is_an_error_not_an_invented_default() {
        let dir = fixture();
        let missing = dir.path().join("missing").display().to_string();
        let error = resolve_search_path(&policy(None, &[missing.as_str()], None))
            .expect_err("no directory");
        assert!(matches!(error, ResolveError::NoUsableDirectories(_)));
    }
}
