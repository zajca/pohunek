//! The one place the daemon builds `git` commands.
//!
//! A daemon `git` child runs against the repository its caller names with
//! `git -C <repo>`, but the environment it inherits can still steer it: a daemon
//! started from a git hook, or from a shell that exported `GIT_DIR`, would
//! address that repository for every project it manages, refuse every ref update
//! because `git receive-pack` left its quarantine marker behind, run the hooks of
//! a `git -c core.hooksPath=...` command, or look for helpers in the `exec-path`
//! of another Git install. The daemon is long-lived and per-invocation Git state
//! must not outlive the command that set it, so [`command`] removes every `GIT_*`
//! variable except the persistent user-level choices in [`INHERITED_GIT_VARS`].

// Rust guideline compliant 2026-10-02

use std::ffi::OsString;
use std::process::Command;

/// Prefix shared by every variable Git reads or exports.
const GIT_VAR_PREFIX: &str = "GIT_";

/// The only `GIT_*` variables a daemon `git` child inherits; each is a
/// persistent choice of the user, not state of one Git invocation.
///
/// - Identity: `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL`, `GIT_COMMITTER_NAME` and
///   `GIT_COMMITTER_EMAIL` name the user who commits.
/// - Configuration files: `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM` and
///   `GIT_CONFIG_NOSYSTEM` choose which files supply the user's configuration;
///   the command-scoped `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT` and
///   `GIT_CONFIG_KEY_<n>`/`GIT_CONFIG_VALUE_<n>` are not allowed.
/// - Transport authentication: `GIT_SSH`, `GIT_SSH_COMMAND`, `GIT_SSH_VARIANT`,
///   `GIT_ASKPASS` and `GIT_TERMINAL_PROMPT` are how fetches authenticate.
/// - TLS trust: `GIT_SSL_CAINFO` and `GIT_SSL_CAPATH` carry a corporate
///   certificate authority; `GIT_SSL_NO_VERIFY` is not allowed because it
///   disables certificate verification.
///
/// Everything else is dropped, including the repository-location variables
/// (`GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, ...), the shallow, graft and
/// replace overrides, `GIT_CEILING_DIRECTORIES`, `GIT_QUARANTINE_PATH`,
/// `GIT_EXEC_PATH` (its helpers would bypass the `PATH` trust check of
/// [`trusted_program`](crate::agent::trusted_program)) and `GIT_TRACE*` (which
/// can write to arbitrary files).
pub(crate) const INHERITED_GIT_VARS: &[&str] = &[
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_SSH_VARIANT",
    "GIT_ASKPASS",
    "GIT_TERMINAL_PROMPT",
    "GIT_SSL_CAINFO",
    "GIT_SSL_CAPATH",
];

/// Names among `names` that a daemon `git` child must not inherit: every
/// `GIT_*` name outside [`INHERITED_GIT_VARS`].
fn dropped_git_vars(names: impl IntoIterator<Item = OsString>) -> impl Iterator<Item = OsString> {
    names.into_iter().filter(|name| {
        name.as_encoded_bytes()
            .starts_with(GIT_VAR_PREFIX.as_bytes())
            && !name
                .to_str()
                .is_some_and(|name| INHERITED_GIT_VARS.contains(&name))
    })
}

/// Builds a command for the trusted `git` executable that drops every `GIT_*`
/// variable of the daemon's environment except [`INHERITED_GIT_VARS`].
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
    for name in dropped_git_vars(std::env::vars_os().map(|(name, _)| name)) {
        command.env_remove(name);
    }
    Ok(command)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    #[test]
    fn the_builder_drops_exactly_the_ambient_git_variables_outside_the_allow_list() {
        let command = command().expect("git on PATH");
        let mut removed: Vec<String> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .filter_map(|(name, _)| name.to_str().map(str::to_owned))
            .collect();
        removed.sort_unstable();
        let mut expected: Vec<String> = std::env::vars_os()
            .filter_map(|(name, _)| name.into_string().ok())
            .filter(|name| {
                name.starts_with(GIT_VAR_PREFIX) && !INHERITED_GIT_VARS.contains(&name.as_str())
            })
            .collect();
        expected.sort_unstable();
        assert_eq!(removed, expected);
        assert_eq!(command.get_envs().count(), expected.len());
    }

    #[test]
    fn every_git_variable_outside_the_allow_list_is_dropped_and_others_are_untouched() {
        let mut names: Vec<OsString> = INHERITED_GIT_VARS.iter().map(OsString::from).collect();
        let allowed = names.len();
        let hostile = [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_CEILING_DIRECTORIES",
            "GIT_QUARANTINE_PATH",
            "GIT_EXEC_PATH",
            "GIT_TRACE",
            "GIT_TRACE2_EVENT",
            "GIT_SSL_NO_VERIFY",
            "GIT_CONFIG",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
            "GIT_SHALLOW_FILE",
            "GIT_GRAFT_FILE",
            "GIT_REPLACE_REF_BASE",
            "GIT_NO_REPLACE_OBJECTS",
            "GIT_PREFIX",
            "GIT_EDITOR",
            "GIT_SOMETHING_FUTURE",
        ];
        names.extend(hostile.map(OsString::from));
        names.extend(["PATH", "HOME", "XGIT_DIR", "git_dir", "GITHUB_TOKEN"].map(OsString::from));
        let dropped: Vec<OsString> = dropped_git_vars(names).collect();
        assert_eq!(dropped, hostile.map(OsString::from));
        assert_eq!(INHERITED_GIT_VARS.len(), allowed);
    }

    #[test]
    fn the_allow_list_holds_only_git_variables_without_duplicates() {
        for name in INHERITED_GIT_VARS {
            assert!(name.starts_with(GIT_VAR_PREFIX), "{name}");
            assert_eq!(
                INHERITED_GIT_VARS.iter().filter(|n| *n == name).count(),
                1,
                "{name} is listed twice"
            );
        }
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
    fn the_allow_list_contains_no_variable_xtask_treats_as_redirecting() {
        let xtask = pohunek_test_support::workspace_root().join("crates/xtask/src/affected.rs");
        let source = std::fs::read_to_string(xtask).expect("read the xtask affected module");
        let theirs = literal_list(&source, "const REPOSITORY_REDIRECTING_VARS")
            .expect("xtask declares REPOSITORY_REDIRECTING_VARS");
        assert!(!theirs.is_empty());
        for name in &theirs {
            assert!(
                !INHERITED_GIT_VARS.contains(&name.as_str()),
                "the daemon allow-list inherits {name}, which xtask drops"
            );
        }
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
