//! The one place the daemon builds `git` commands.
//!
//! A daemon `git` child runs against the repository its caller names with
//! `git -C <repo>`, but `-C` does not override the repository-location
//! variables of the environment: a daemon started from a git hook, or from a
//! shell that exported `GIT_DIR`, would otherwise address that repository for
//! every project it manages.

// Rust guideline compliant 2026-10-02

use std::process::Command;

/// Variables that move Git to another repository, index or object store than
/// the one in the working directory, or that substitute the history it reads
/// from that repository.
///
/// These are the repository-location variables of git(1) "ENVIRONMENT
/// VARIABLES" ("The Git Repository") and the repository-local variables that
/// `git rev-parse --local-env-vars` lists. Git sets `GIT_DIR` and
/// `GIT_INDEX_FILE` itself while it runs a hook, and `GIT_REFERENCE_BACKEND`
/// overrides the repository's ref storage format. `GIT_SHALLOW_FILE` and
/// `GIT_GRAFT_FILE` name another shallow boundary or graft list, and
/// `GIT_REPLACE_REF_BASE` and `GIT_NO_REPLACE_OBJECTS` change which replace refs
/// apply; each changes the commits `rev-list`, `rev-parse <commit>^` and
/// `fetch` see. `GIT_IMPLICIT_WORK_TREE` stays: it only qualifies an explicit
/// `GIT_DIR`, which is removed. `GIT_PREFIX` stays: Git only exports it to
/// aliases and hooks and never reads it. `GIT_CONFIG`, `GIT_CONFIG_COUNT` and
/// `GIT_CONFIG_PARAMETERS` stay because they are configuration input, like the
/// other `GIT_CONFIG_*` variables. `GIT_DISCOVERY_ACROSS_FILESYSTEM` stays:
/// unset is Git's default (stop at a filesystem boundary) and a set value only
/// widens discovery, so removing it could hide a repository across a mount.
/// `GIT_DEFAULT_HASH`, `GIT_DEFAULT_REF_FORMAT` and `GIT_INDEX_VERSION` stay
/// because they only shape a repository or index while Git creates it and never
/// redirect a command at an existing repository. The user's configuration and
/// identity variables (`GIT_CONFIG_*`, `GIT_AUTHOR_*`, `GIT_COMMITTER_*`,
/// `GIT_SSH_COMMAND`, ...) stay because they are legitimate input.
///
/// `xtask` keeps this list for its changed-file queries
/// (`REPOSITORY_REDIRECTING_VARS` in `crates/xtask/src/affected.rs`); a test
/// here keeps it equal, because no crate both depend on is a sensible home for
/// a list of Git variable names. The daemon additionally drops
/// [`DISCOVERY_RESTRICTING_VARS`].
pub(crate) const REPOSITORY_REDIRECTING_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_COMMON_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_REFERENCE_BACKEND",
    "GIT_SHALLOW_FILE",
    "GIT_GRAFT_FILE",
    "GIT_REPLACE_REF_BASE",
    "GIT_NO_REPLACE_OBJECTS",
];

/// Variables that only restrict where Git looks for a repository.
///
/// `GIT_CEILING_DIRECTORIES` stops discovery at the listed directories. A daemon
/// runs git with `-C <session cwd>`, often a nested directory of the project, so
/// an inherited ceiling at the project root hides the project from detection;
/// the daemon's own environment is incidental to the sessions it serves. `xtask`
/// does not drop it because it always runs git from the repository root.
pub(crate) const DISCOVERY_RESTRICTING_VARS: &[&str] = &["GIT_CEILING_DIRECTORIES"];

/// Builds a command for the trusted `git` executable that drops the
/// [`REPOSITORY_REDIRECTING_VARS`] and [`DISCOVERY_RESTRICTING_VARS`] it would
/// inherit.
///
/// The executable is resolved once through
/// [`trusted_program`](crate::agent::trusted_program), so no second `PATH`
/// search can pick another candidate.
///
/// # Errors
///
/// Returns the not-found-or-untrusted message of `trusted_program`, prefixed
/// like a failed spawn.
pub(crate) fn command() -> Result<Command, String> {
    let program =
        crate::agent::trusted_program("git").map_err(|err| format!("failed to run git: {err}"))?;
    let mut command = Command::new(program);
    for var in REPOSITORY_REDIRECTING_VARS
        .iter()
        .chain(DISCOVERY_RESTRICTING_VARS)
    {
        command.env_remove(var);
    }
    Ok(command)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    #[test]
    fn the_builder_drops_repository_redirecting_variables_and_nothing_else() {
        let command = command().expect("git on PATH");
        let mut removed: Vec<&str> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .filter_map(|(name, _)| name.to_str())
            .collect();
        removed.sort_unstable();
        let mut expected = REPOSITORY_REDIRECTING_VARS.to_vec();
        expected.extend(DISCOVERY_RESTRICTING_VARS);
        expected.sort_unstable();
        assert_eq!(removed, expected);
        assert_eq!(command.get_envs().count(), expected.len());
    }

    #[test]
    fn the_list_names_only_repository_location_variables() {
        for kept in [
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_PARAMETERS",
            "GIT_PREFIX",
            "GIT_IMPLICIT_WORK_TREE",
            "GIT_AUTHOR_NAME",
            "GIT_COMMITTER_EMAIL",
            "GIT_DISCOVERY_ACROSS_FILESYSTEM",
            "GIT_DEFAULT_HASH",
            "GIT_DEFAULT_REF_FORMAT",
            "GIT_INDEX_VERSION",
        ] {
            assert!(!REPOSITORY_REDIRECTING_VARS.contains(&kept), "{kept}");
            assert!(!DISCOVERY_RESTRICTING_VARS.contains(&kept), "{kept}");
        }
        assert_eq!(REPOSITORY_REDIRECTING_VARS.len(), 12);
        assert_eq!(DISCOVERY_RESTRICTING_VARS, ["GIT_CEILING_DIRECTORIES"]);
    }

    /// Names in the string-literal list that follows `marker` in `source`.
    fn literal_list(source: &str, marker: &str) -> Option<Vec<String>> {
        let start = source.find(marker)?;
        let body = &source[source[start..].find('[')? + start + 1..];
        let body = &body[..body.find("];")?];
        Some(
            body.split('"')
                .skip(1)
                .step_by(2)
                .map(str::to_owned)
                .collect(),
        )
    }

    #[test]
    fn the_list_contains_every_xtask_variable_and_only_the_ceiling_is_daemon_only() {
        let xtask = pohunek_test_support::workspace_root().join("crates/xtask/src/affected.rs");
        let source = std::fs::read_to_string(xtask).expect("read the xtask affected module");
        let theirs = literal_list(&source, "const REPOSITORY_REDIRECTING_VARS")
            .expect("xtask declares REPOSITORY_REDIRECTING_VARS");
        let ours: Vec<&str> = REPOSITORY_REDIRECTING_VARS
            .iter()
            .chain(DISCOVERY_RESTRICTING_VARS)
            .copied()
            .collect();
        for name in &theirs {
            assert!(ours.contains(&name.as_str()), "daemon list lacks {name}");
        }
        // The daemon runs git from nested session directories, where an
        // inherited ceiling hides the project; xtask runs from the repository
        // root, where it cannot.
        let daemon_only: Vec<&str> = ours
            .iter()
            .copied()
            .filter(|name| !theirs.iter().any(|theirs| theirs == name))
            .collect();
        assert_eq!(daemon_only, ["GIT_CEILING_DIRECTORIES"]);
    }

    /// Product code outside this module that resolves or spawns `git`.
    fn product_git_spawns() -> Vec<(PathBuf, String)> {
        fn walk(dir: &Path, found: &mut Vec<(PathBuf, String)>) {
            for entry in std::fs::read_dir(dir).expect("read source directory") {
                let path = entry.expect("directory entry").path();
                if path.is_dir() {
                    walk(&path, found);
                    continue;
                }
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if path.extension().is_none_or(|ext| ext != "rs")
                    || name == "git.rs"
                    || name.ends_with("tests.rs")
                    || name == "test_support.rs"
                {
                    continue;
                }
                let source = std::fs::read_to_string(&path).expect("read source file");
                let product = source
                    .find("\n#[cfg(test)]\nmod tests")
                    .map_or(source.as_str(), |end| &source[..end]);
                for pattern in ["Command::new(\"git\")", "trusted_program(\"git\")"] {
                    if product.contains(pattern) {
                        found.push((path.clone(), pattern.to_owned()));
                    }
                }
            }
        }
        let mut found = Vec::new();
        walk(
            &pohunek_test_support::manifest_dir().join("src"),
            &mut found,
        );
        found
    }

    #[test]
    fn no_product_code_spawns_git_outside_this_module() {
        let found = product_git_spawns();
        assert!(
            found.is_empty(),
            "git spawned outside crate::git: {found:?}"
        );
    }
}
