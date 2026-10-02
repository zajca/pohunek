//! The one place the daemon builds `git` commands.
//!
//! A daemon `git` child runs against the repository its caller names with
//! `git -C <repo>`, but `-C` does not override the repository-location
//! variables of the environment: a daemon started from a git hook, or from a
//! shell that exported `GIT_DIR`, would otherwise address that repository for
//! every project it manages, or refuse every ref update because a
//! `git receive-pack` hook left its quarantine marker behind. Configuration
//! that `git -c` handed to a hook through the environment is dropped for the
//! same reason: it describes one command, not the daemon's life.

// Rust guideline compliant 2026-10-02

use std::ffi::OsString;
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
/// aliases and hooks and never reads it. `GIT_CONFIG` stays: only `git config`
/// reads it, and no daemon child is that command. The command-scoped
/// configuration variables are in [`COMMAND_SCOPED_CONFIG_VARS`], while
/// `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM` and `GIT_CONFIG_NOSYSTEM` stay
/// because they are the user's persistent choice of configuration files.
/// `GIT_DISCOVERY_ACROSS_FILESYSTEM` stays:
/// unset is Git's default (stop at a filesystem boundary) and a set value only
/// widens discovery, so removing it could hide a repository across a mount.
/// `GIT_DEFAULT_HASH`, `GIT_DEFAULT_REF_FORMAT` and `GIT_INDEX_VERSION` stay
/// because they only shape a repository or index while Git creates it and never
/// redirect a command at an existing repository. The user's configuration and
/// identity variables (`GIT_CONFIG_GLOBAL`, `GIT_AUTHOR_*`, `GIT_COMMITTER_*`,
/// `GIT_SSH_COMMAND`, ...) stay because they are legitimate input.
///
/// `xtask` keeps this list for its changed-file queries
/// (`REPOSITORY_REDIRECTING_VARS` in `crates/xtask/src/affected.rs`); a test
/// here keeps it equal, because no crate both depend on is a sensible home for
/// a list of Git variable names. The daemon additionally drops
/// [`DISCOVERY_RESTRICTING_VARS`] and [`WRITE_RESTRICTING_VARS`].
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

/// Variables Git exports to a hook that forbid the hook's children from
/// updating refs.
///
/// `git receive-pack` sets `GIT_QUARANTINE_PATH` while it runs `pre-receive` and
/// `update` hooks; with it present, every ref update of a child fails with "ref
/// updates forbidden inside quarantine environment", which breaks
/// `git worktree add -b` for the daemon's whole life. `xtask` only reads, so it
/// does not drop it.
///
/// The other variables Git sets for a hook (githooks(5), git-receive-pack(1))
/// stay, because none changes what a daemon child reads or writes:
/// `GIT_PUSH_OPTION_COUNT`, `GIT_PUSH_OPTION_<n>` and the `GIT_PUSH_CERT*`
/// family are data that only a hook consumes; `GIT_REFLOG_ACTION` only labels
/// reflog entries; `GIT_EDITOR` and `GIT_SEQUENCE_EDITOR` are never run because
/// the daemon passes no command that opens an editor; `GIT_EXEC_PATH` names the
/// helper directory of the Git that set it and is as trusted as `PATH`;
/// `GIT_TRACE*` only writes diagnostics; `GIT_PAGER_IN_USE` only affects
/// output decoration; `GIT_TEMPLATE_DIR` is read by `init` and `clone`, which
/// the daemon does not run; `GIT_SENDEMAIL_FILE_*` and `GIT_DIFF_PATH_*` are
/// handed to `send-email` hooks and external diff programs only. The
/// repository-location variables Git also sets (`GIT_DIR`, `GIT_INDEX_FILE`,
/// `GIT_OBJECT_DIRECTORY`, `GIT_ALTERNATE_OBJECT_DIRECTORIES`,
/// `GIT_WORK_TREE`) are in [`REPOSITORY_REDIRECTING_VARS`].
pub(crate) const WRITE_RESTRICTING_VARS: &[&str] = &["GIT_QUARANTINE_PATH"];

/// Variables through which Git hands the configuration of one command to the
/// processes it starts.
///
/// `git -c key=value` exports `GIT_CONFIG_PARAMETERS` to hooks and aliases, and
/// `GIT_CONFIG_COUNT` with `GIT_CONFIG_KEY_<n>` and `GIT_CONFIG_VALUE_<n>` carry
/// the same kind of override. A daemon started from such a hook would apply it
/// to every repository it manages for its whole life, for example a
/// `core.hooksPath` that runs the original repository's hooks in other
/// checkouts or a `protocol.file.allow=never` that breaks fetches from local
/// remotes, so a command-scoped override must not outlive the command that set
/// it. Without `GIT_CONFIG_COUNT` the key and value variables are inert; they
/// are still dropped by name (see [`command`]) so no later count can revive
/// them. `xtask` is a one-shot developer command where such an override is the
/// invoking user's intent, so it keeps them.
pub(crate) const COMMAND_SCOPED_CONFIG_VARS: &[&str] =
    &["GIT_CONFIG_PARAMETERS", "GIT_CONFIG_COUNT"];

/// Prefixes of the indexed `GIT_CONFIG_COUNT` entries.
const COMMAND_SCOPED_CONFIG_ENTRY_PREFIXES: [&str; 2] = ["GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"];

/// Names among `names` that are indexed command-scoped configuration entries.
fn command_scoped_config_entries(
    names: impl IntoIterator<Item = OsString>,
) -> impl Iterator<Item = OsString> {
    names.into_iter().filter(|name| {
        name.to_str().is_some_and(|name| {
            COMMAND_SCOPED_CONFIG_ENTRY_PREFIXES
                .iter()
                .any(|prefix| name.starts_with(prefix))
        })
    })
}

/// Builds a command for the trusted `git` executable that drops the
/// [`REPOSITORY_REDIRECTING_VARS`], [`DISCOVERY_RESTRICTING_VARS`],
/// [`WRITE_RESTRICTING_VARS`] and [`COMMAND_SCOPED_CONFIG_VARS`] it would
/// inherit, plus every `GIT_CONFIG_KEY_<n>` and `GIT_CONFIG_VALUE_<n>` present
/// in the daemon's environment.
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
        .chain(WRITE_RESTRICTING_VARS)
        .chain(COMMAND_SCOPED_CONFIG_VARS)
    {
        command.env_remove(var);
    }
    for entry in command_scoped_config_entries(std::env::vars_os().map(|(name, _)| name)) {
        command.env_remove(entry);
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
        expected.extend(WRITE_RESTRICTING_VARS);
        expected.extend(COMMAND_SCOPED_CONFIG_VARS);
        // Indexed configuration entries present in this process's environment
        // are dropped as well.
        let entries: Vec<String> =
            command_scoped_config_entries(std::env::vars_os().map(|(name, _)| name))
                .filter_map(|name| name.into_string().ok())
                .collect();
        expected.extend(entries.iter().map(String::as_str));
        expected.sort_unstable();
        expected.dedup();
        assert_eq!(removed, expected);
        assert_eq!(command.get_envs().count(), expected.len());
    }

    #[test]
    fn indexed_configuration_entries_are_selected_by_prefix_only() {
        let names = [
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
            "GIT_CONFIG_KEY_17",
            "GIT_CONFIG_VALUE_17",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_NOSYSTEM",
            "GIT_CONFIG",
            "GIT_CONFIG_COUNT",
            "PATH",
        ]
        .map(OsString::from);
        let selected: Vec<OsString> = command_scoped_config_entries(names).collect();
        assert_eq!(
            selected,
            [
                "GIT_CONFIG_KEY_0",
                "GIT_CONFIG_VALUE_0",
                "GIT_CONFIG_KEY_17",
                "GIT_CONFIG_VALUE_17"
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn the_list_names_only_repository_location_variables() {
        for kept in [
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_NOSYSTEM",
            "GIT_CONFIG",
            "GIT_PREFIX",
            "GIT_IMPLICIT_WORK_TREE",
            "GIT_AUTHOR_NAME",
            "GIT_COMMITTER_EMAIL",
            "GIT_DISCOVERY_ACROSS_FILESYSTEM",
            "GIT_DEFAULT_HASH",
            "GIT_DEFAULT_REF_FORMAT",
            "GIT_INDEX_VERSION",
            "GIT_PUSH_OPTION_COUNT",
            "GIT_PUSH_CERT",
            "GIT_REFLOG_ACTION",
            "GIT_EXEC_PATH",
            "GIT_EDITOR",
        ] {
            assert!(!REPOSITORY_REDIRECTING_VARS.contains(&kept), "{kept}");
            assert!(!DISCOVERY_RESTRICTING_VARS.contains(&kept), "{kept}");
            assert!(!WRITE_RESTRICTING_VARS.contains(&kept), "{kept}");
            assert!(!COMMAND_SCOPED_CONFIG_VARS.contains(&kept), "{kept}");
        }
        assert_eq!(REPOSITORY_REDIRECTING_VARS.len(), 12);
        assert_eq!(DISCOVERY_RESTRICTING_VARS, ["GIT_CEILING_DIRECTORIES"]);
        assert_eq!(WRITE_RESTRICTING_VARS, ["GIT_QUARANTINE_PATH"]);
        assert_eq!(
            COMMAND_SCOPED_CONFIG_VARS,
            ["GIT_CONFIG_PARAMETERS", "GIT_CONFIG_COUNT"]
        );
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
    fn the_list_contains_every_xtask_variable_and_only_session_and_hook_variables_are_daemon_only()
    {
        let xtask = pohunek_test_support::workspace_root().join("crates/xtask/src/affected.rs");
        let source = std::fs::read_to_string(xtask).expect("read the xtask affected module");
        let theirs = literal_list(&source, "const REPOSITORY_REDIRECTING_VARS")
            .expect("xtask declares REPOSITORY_REDIRECTING_VARS");
        let ours: Vec<&str> = REPOSITORY_REDIRECTING_VARS
            .iter()
            .chain(DISCOVERY_RESTRICTING_VARS)
            .chain(WRITE_RESTRICTING_VARS)
            .chain(COMMAND_SCOPED_CONFIG_VARS)
            .copied()
            .collect();
        for name in &theirs {
            assert!(ours.contains(&name.as_str()), "daemon list lacks {name}");
        }
        // The daemon runs git from nested session directories, where an
        // inherited ceiling hides the project, and writes refs, which a
        // quarantine marker forbids, and outlives the hook whose command-scoped
        // configuration it could inherit; xtask runs once from the repository
        // root, only reads, and honors the invoking user's `git -c`.
        let daemon_only: Vec<&str> = ours
            .iter()
            .copied()
            .filter(|name| !theirs.iter().any(|theirs| theirs == name))
            .collect();
        assert_eq!(
            daemon_only,
            [
                "GIT_CEILING_DIRECTORIES",
                "GIT_QUARANTINE_PATH",
                "GIT_CONFIG_PARAMETERS",
                "GIT_CONFIG_COUNT"
            ]
        );
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
