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
//! variable except the persistent user choices named by [`is_inherited`].

// Rust guideline compliant 2026-10-02

use std::ffi::OsString;
use std::process::Command;

/// Prefix shared by every variable Git reads or exports.
const GIT_VAR_PREFIX: &str = "GIT_";

/// Who commits: `git commit` and `git worktree` operations read these.
const IDENTITY_VARS: &[&str] = &[
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
];

/// Which files supply the user's configuration; the command-scoped
/// `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT` and `GIT_CONFIG_KEY_<n>`/
/// `GIT_CONFIG_VALUE_<n>` are not among them.
const CONFIG_FILE_VARS: &[&str] = &[
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
];

/// How fetches reach and authenticate to a remote.
const TRANSPORT_VARS: &[&str] = &[
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_SSH_VARIANT",
    "GIT_PROXY_COMMAND",
    "GIT_ASKPASS",
    "GIT_TERMINAL_PROMPT",
];

/// TLS trust and client authentication of HTTPS remotes; the user's certificate
/// authority, client certificate and key (with the `*_TYPE` of a P12/DER
/// certificate or an ENG/PKCS#11 key), and protocol or cipher policy.
/// `GIT_SSL_NO_VERIFY` is not among them because it disables verification.
const TLS_VARS: &[&str] = &[
    "GIT_SSL_CAINFO",
    "GIT_SSL_CAPATH",
    "GIT_SSL_CERT",
    "GIT_SSL_KEY",
    "GIT_SSL_CERT_TYPE",
    "GIT_SSL_KEY_TYPE",
    "GIT_SSL_CERT_PASSWORD_PROTECTED",
    "GIT_SSL_VERSION",
    "GIT_SSL_CIPHER_LIST",
    "GIT_PROXY_SSL_CAINFO",
    "GIT_PROXY_SSL_CERT",
    "GIT_PROXY_SSL_KEY",
    "GIT_PROXY_SSL_CERT_PASSWORD_PROTECTED",
];

/// HTTP proxy authentication and the user's connection tuning.
const HTTP_VARS: &[&str] = &[
    "GIT_HTTP_PROXY_AUTHMETHOD",
    "GIT_HTTP_LOW_SPEED_LIMIT",
    "GIT_HTTP_LOW_SPEED_TIME",
    "GIT_HTTP_USER_AGENT",
    "GIT_HTTP_MAX_REQUESTS",
    "GIT_HTTP_RETRY_AFTER",
    "GIT_HTTP_MAX_RETRIES",
    "GIT_HTTP_MAX_RETRY_TIME",
    "GIT_CURL_FTP_NO_EPSV",
];

/// Which transport protocols Git may use. `GIT_PROTOCOL_FROM_USER` is not among
/// them: Git sets it for its own subprocesses to mark a URL as untrusted, and an
/// inherited `0` would refuse the `user`-allowed protocols of the daemon's own
/// fetches.
const PROTOCOL_VARS: &[&str] = &["GIT_ALLOW_PROTOCOL"];

/// Repository detection across a mount boundary, which the user opts into for a
/// project that spans filesystems.
const DISCOVERY_VARS: &[&str] = &["GIT_DISCOVERY_ACROSS_FILESYSTEM"];

/// Every group of exact names a daemon `git` child inherits.
const INHERITED_GIT_VAR_GROUPS: &[&[&str]] = &[
    IDENTITY_VARS,
    CONFIG_FILE_VARS,
    TRANSPORT_VARS,
    TLS_VARS,
    HTTP_VARS,
    PROTOCOL_VARS,
    DISCOVERY_VARS,
];

/// Prefixes of variable families a daemon `git` child inherits whole: the
/// `GIT_LFS_*` settings of git-lfs, whose filters run during checkout and fetch.
const INHERITED_GIT_PREFIXES: &[&str] = &["GIT_LFS_"];

/// Names inside an inherited prefix family that still name a file the tool
/// writes to, like the `GIT_TRACE*` diagnostics: `GIT_LFS_PROGRESS`.
const DIAGNOSTIC_FILE_VARS: &[&str] = &["GIT_LFS_PROGRESS"];

/// Whether a daemon `git` child inherits the variable `name`.
///
/// Inherited variables are persistent user choices: the groups above and the
/// [`INHERITED_GIT_PREFIXES`] families. Every other `GIT_*` name is either state
/// of one Git invocation (repository location, quarantine, command-scoped
/// configuration, `GIT_EXEC_PATH` whose helpers would bypass the `PATH` trust
/// check of [`trusted_program`](crate::agent::trusted_program), the prefix,
/// reflog action and editor hand-offs a parent Git exports) or a diagnostic that
/// can write to arbitrary files (`GIT_TRACE*`, `GIT_CURL_VERBOSE`,
/// [`DIAGNOSTIC_FILE_VARS`]).
///
/// `GIT_CEILING_DIRECTORIES` is dropped because the daemon serves many projects
/// and a ceiling inherited from the environment that launched it is not a
/// setting about them: a hook or a shell can carry one that would hide a
/// project's repository from detection.
pub(crate) fn is_inherited(name: &str) -> bool {
    if DIAGNOSTIC_FILE_VARS.contains(&name) {
        return false;
    }
    INHERITED_GIT_VAR_GROUPS
        .iter()
        .any(|group| group.contains(&name))
        || INHERITED_GIT_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// Names among `names` that a daemon `git` child must not inherit: every
/// `GIT_*` name for which [`is_inherited`] is false.
fn dropped_git_vars(names: impl IntoIterator<Item = OsString>) -> impl Iterator<Item = OsString> {
    names.into_iter().filter(|name| {
        name.as_encoded_bytes()
            .starts_with(GIT_VAR_PREFIX.as_bytes())
            && !name.to_str().is_some_and(is_inherited)
    })
}

/// Builds a command for the trusted `git` executable that drops every `GIT_*`
/// variable of the daemon's environment except those [`is_inherited`] accepts.
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
            .filter(|name| name.starts_with(GIT_VAR_PREFIX) && !is_inherited(name))
            .collect();
        expected.sort_unstable();
        assert_eq!(removed, expected);
        assert_eq!(command.get_envs().count(), expected.len());
    }

    /// Persistent user choices of every kept group, plus one `GIT_LFS_` name.
    fn kept_names() -> Vec<&'static str> {
        let mut names: Vec<&str> = INHERITED_GIT_VAR_GROUPS
            .iter()
            .flat_map(|group| group.iter().copied())
            .collect();
        names.extend(["GIT_LFS_SKIP_SMUDGE", "GIT_LFS_CONCURRENT_TRANSFERS"]);
        names
    }

    #[test]
    fn every_persistent_user_choice_is_kept() {
        let named = [
            "GIT_AUTHOR_NAME",
            "GIT_COMMITTER_EMAIL",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_NOSYSTEM",
            "GIT_SSH",
            "GIT_SSH_COMMAND",
            "GIT_SSH_VARIANT",
            "GIT_PROXY_COMMAND",
            "GIT_ASKPASS",
            "GIT_TERMINAL_PROMPT",
            "GIT_SSL_CAINFO",
            "GIT_SSL_CAPATH",
            "GIT_SSL_CERT",
            "GIT_SSL_KEY",
            "GIT_SSL_CERT_TYPE",
            "GIT_SSL_KEY_TYPE",
            "GIT_SSL_CERT_PASSWORD_PROTECTED",
            "GIT_SSL_VERSION",
            "GIT_SSL_CIPHER_LIST",
            "GIT_PROXY_SSL_CAINFO",
            "GIT_PROXY_SSL_CERT",
            "GIT_PROXY_SSL_KEY",
            "GIT_PROXY_SSL_CERT_PASSWORD_PROTECTED",
            "GIT_HTTP_PROXY_AUTHMETHOD",
            "GIT_HTTP_LOW_SPEED_LIMIT",
            "GIT_HTTP_LOW_SPEED_TIME",
            "GIT_HTTP_USER_AGENT",
            "GIT_HTTP_MAX_REQUESTS",
            "GIT_HTTP_RETRY_AFTER",
            "GIT_HTTP_MAX_RETRIES",
            "GIT_HTTP_MAX_RETRY_TIME",
            "GIT_ALLOW_PROTOCOL",
            "GIT_DISCOVERY_ACROSS_FILESYSTEM",
            "GIT_LFS_SKIP_SMUDGE",
            "GIT_LFS_",
        ];
        for name in named {
            assert!(is_inherited(name), "{name} must be kept");
        }
    }

    #[test]
    fn per_invocation_state_and_diagnostics_are_dropped_and_others_are_untouched() {
        let mut names: Vec<OsString> = kept_names().into_iter().map(OsString::from).collect();
        let hostile = [
            "GIT_DIR",
            "GIT_COMMON_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
            "GIT_CEILING_DIRECTORIES",
            "GIT_QUARANTINE_PATH",
            "GIT_EXEC_PATH",
            "GIT_PREFIX",
            "GIT_REFLOG_ACTION",
            "GIT_PROTOCOL",
            "GIT_PROTOCOL_FROM_USER",
            "GIT_PUSH_CERT",
            "GIT_PROJECT_ROOT",
            "GIT_EDITOR",
            "GIT_SEQUENCE_EDITOR",
            "GIT_PAGER",
            "GIT_EXTERNAL_DIFF",
            "GIT_TEMPLATE_DIR",
            "GIT_AUTHOR_DATE",
            "GIT_TRACE",
            "GIT_TRACE2_EVENT",
            "GIT_TRACE_PACKET",
            "GIT_TRACE_CURL",
            "GIT_CURL_VERBOSE",
            "GIT_LFS_PROGRESS",
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
            "GIT_REFERENCE_BACKEND",
            "GIT_TEST_FSYNC",
            "GIT_LFS",
            "GIT_SOMETHING_FUTURE",
        ];
        names.extend(hostile.map(OsString::from));
        names.extend(
            [
                "PATH",
                "HOME",
                "XGIT_DIR",
                "git_dir",
                "GITHUB_TOKEN",
                "XGIT_LFS_X",
            ]
            .map(OsString::from),
        );
        let dropped: Vec<OsString> = dropped_git_vars(names).collect();
        assert_eq!(dropped, hostile.map(OsString::from));
    }

    #[test]
    fn the_kept_groups_hold_only_git_variables_without_duplicates() {
        let names = kept_names();
        for name in &names {
            assert!(name.starts_with(GIT_VAR_PREFIX), "{name}");
            assert_eq!(
                names.iter().filter(|n| *n == name).count(),
                1,
                "{name} is listed twice"
            );
        }
        for prefix in INHERITED_GIT_PREFIXES {
            assert!(prefix.starts_with(GIT_VAR_PREFIX) && prefix.ends_with('_'));
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
                !is_inherited(name),
                "the daemon inherits {name}, which xtask drops"
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
