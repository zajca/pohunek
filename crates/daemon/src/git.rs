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
/// the one in the working directory.
///
/// These are the repository-location variables of git(1) "ENVIRONMENT
/// VARIABLES" ("The Git Repository"). Git sets `GIT_DIR` and `GIT_INDEX_FILE`
/// itself while it runs a hook. The discovery limits
/// (`GIT_CEILING_DIRECTORIES`, `GIT_DISCOVERY_ACROSS_FILESYSTEM`) stay because
/// every daemon command starts at the repository it names, and the user's
/// configuration and identity variables (`GIT_CONFIG_*`, `GIT_AUTHOR_*`,
/// `GIT_COMMITTER_*`, `GIT_SSH_COMMAND`, ...) stay because they are legitimate
/// input.
///
/// `xtask` keeps the same list for its changed-file queries
/// (`REPOSITORY_REDIRECTING_VARS` in `crates/xtask/src/affected.rs`); a test
/// here keeps the two equal, because no crate both depend on is a
/// sensible home for a list of Git variable names.
pub(crate) const REPOSITORY_REDIRECTING_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_COMMON_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
];

/// Builds a command for the trusted `git` executable that drops the
/// [`REPOSITORY_REDIRECTING_VARS`] it would inherit.
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
    for var in REPOSITORY_REDIRECTING_VARS {
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
        expected.sort_unstable();
        assert_eq!(removed, expected);
        assert_eq!(command.get_envs().count(), expected.len());
    }

    #[test]
    fn the_list_names_only_repository_location_variables() {
        for kept in [
            "GIT_CONFIG_GLOBAL",
            "GIT_AUTHOR_NAME",
            "GIT_COMMITTER_EMAIL",
        ] {
            assert!(!REPOSITORY_REDIRECTING_VARS.contains(&kept), "{kept}");
        }
        assert_eq!(REPOSITORY_REDIRECTING_VARS.len(), 7);
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
    fn the_list_equals_the_xtask_list() {
        let xtask = pohunek_test_support::workspace_root().join("crates/xtask/src/affected.rs");
        let source = std::fs::read_to_string(xtask).expect("read the xtask affected module");
        let mut theirs = literal_list(&source, "const REPOSITORY_REDIRECTING_VARS")
            .expect("xtask declares REPOSITORY_REDIRECTING_VARS");
        let mut ours: Vec<String> = REPOSITORY_REDIRECTING_VARS
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        ours.sort_unstable();
        theirs.sort_unstable();
        assert_eq!(ours, theirs);
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
