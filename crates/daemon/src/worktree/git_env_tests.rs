//! Daemon `git` children ignore repository-location variables, command-scoped
//! configuration and the helper directory of the environment they inherit, as in
//! a daemon started from a git hook.
//!
//! The variables are set on a child process, because mutating this process's
//! environment would race every other test.

use std::path::{Path, PathBuf};

use pohunek_test_support::env::TestEnv;

use super::{branch_exists, fetch_origin, worktree_add_new, worktree_remove};

/// Variables the child starts with: each points Git at a repository, index or
/// worktree that does not exist.
const AMBIENT_GIT_VARS: [(&str, &str); 6] = [
    ("GIT_DIR", "/nonexistent/pohunek/.git"),
    ("GIT_WORK_TREE", "/nonexistent/pohunek"),
    ("GIT_INDEX_FILE", "/nonexistent/pohunek/index"),
    ("GIT_REFERENCE_BACKEND", "bogus"),
    // Neither replace ref base holds a ref, so replacement is off for a Git
    // that honors them.
    ("GIT_REPLACE_REF_BASE", "refs/nonexistent/"),
    ("GIT_NO_REPLACE_OBJECTS", "1"),
];

/// Variables that keep the host's system and global Git configuration, such as
/// `init.templateDir`, `core.hooksPath` or filters, out of the fixture and the
/// daemon's git commands alike.
const ISOLATED_GIT_CONFIG: [(&str, &str); 2] = [
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
];

/// Variable naming the file the child writes, after it created the commits, that
/// the parent set as `GIT_SHALLOW_FILE`; it hides every parent of `HEAD`.
const SHALLOW_FILE_VAR: &str = "GIT_SHALLOW_FILE";

/// Variable naming the file the child writes that the parent set as
/// `GIT_GRAFT_FILE`; it declares `HEAD` parentless.
const GRAFT_FILE_VAR: &str = "GIT_GRAFT_FILE";

/// Variable naming the existing directory the parent set as
/// `GIT_QUARANTINE_PATH`, as `git receive-pack` does for its hooks; Git refuses
/// every ref update while it is set.
const QUARANTINE_VAR: &str = "GIT_QUARANTINE_PATH";

/// Variable naming the repository the child creates and the parent set as the
/// ceiling, so the ceiling sits at the project root.
const REPO_VAR: &str = "POHUNEK_TEST_GIT_REPO";

/// Runs a setup `git` command with the test environment's scrubbed variables,
/// so the ambient variables of the child never reach the fixture itself, and
/// returns its trimmed stdout.
fn fixture_git(env: &TestEnv, repo: &Path, args: &[&str]) -> String {
    let output = env
        .command("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .envs(ISOLATED_GIT_CONFIG)
        .output()
        .expect("run fixture git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("fixture git output is UTF-8")
        .trim()
        .to_owned()
}

/// Child half of [`ambient_git_variables_do_not_redirect_daemon_git`].
#[test]
#[ignore = "child process of ambient_git_variables_do_not_redirect_daemon_git"]
fn daemon_git_under_ambient_git_variables() {
    let env = TestEnv::new().expect("hermetic test environment");
    let repo = std::path::PathBuf::from(std::env::var_os(REPO_VAR).expect("repository path"));
    std::fs::create_dir(&repo).expect("create repository directory");
    fixture_git(&env, &repo, &["init", "-q", "-b", "main"]);
    fixture_git(&env, &repo, &["config", "user.email", "test@example.com"]);
    fixture_git(&env, &repo, &["config", "user.name", "Test"]);
    fixture_git(&env, &repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("README.md"), "init\n").expect("write file");
    fixture_git(&env, &repo, &["add", "."]);
    fixture_git(&env, &repo, &["commit", "-q", "-m", "init"]);
    for message in ["second", "third"] {
        fixture_git(
            &env,
            &repo,
            &["commit", "-q", "--allow-empty", "-m", message],
        );
    }
    let head = fixture_git(&env, &repo, &["rev-parse", "HEAD"]);
    assert_eq!(
        fixture_git(&env, &repo, &["rev-list", "--count", "HEAD"]),
        "3"
    );

    // The ambient shallow and graft files each cut the history of `HEAD` to one
    // commit for a Git that honors them.
    let shallow = PathBuf::from(std::env::var_os(SHALLOW_FILE_VAR).expect("shallow file path"));
    let graft = PathBuf::from(std::env::var_os(GRAFT_FILE_VAR).expect("graft file path"));
    std::fs::write(&shallow, format!("{head}\n")).expect("write shallow file");
    std::fs::write(&graft, format!("{head}\n")).expect("write graft file");
    assert_eq!(
        crate::project::detect::git(&repo, &["rev-list", "--count", "HEAD"]).as_deref(),
        Some("3"),
        "the daemon sees the full history"
    );
    assert!(
        crate::project::detect::git(&repo, &["rev-parse", "--verify", "HEAD^"]).is_some(),
        "the parent of HEAD resolves"
    );
    std::fs::remove_file(&shallow).expect("remove shallow file");
    std::fs::remove_file(&graft).expect("remove graft file");

    // Project detection addresses the project's repository.
    let toplevel = crate::project::detect::git(&repo, &["rev-parse", "--show-toplevel"])
        .expect("detection reaches the project repository");
    assert_eq!(
        std::fs::canonicalize(toplevel).expect("canonical toplevel"),
        std::fs::canonicalize(&repo).expect("canonical repository")
    );

    // `GIT_CEILING_DIRECTORIES` names the project root, and session directories
    // are often nested inside the project: detection still finds it.
    let nested = repo.join("crates/daemon/src");
    std::fs::create_dir_all(&nested).expect("create nested directory");
    let toplevel = crate::project::detect::git(&nested, &["rev-parse", "--show-toplevel"])
        .expect("detection from a nested directory reaches the project repository");
    assert_eq!(
        std::fs::canonicalize(toplevel).expect("canonical toplevel"),
        std::fs::canonicalize(&repo).expect("canonical repository")
    );

    // A worktree operation addresses the same repository.
    let checkout = env.root().join("checkout");
    worktree_add_new(&repo, &checkout, "feature", "main").expect("add worktree");
    assert!(checkout.join("README.md").is_file());
    assert_eq!(branch_exists(&repo, "feature"), Ok(true));
    worktree_remove(&repo, &checkout).expect("remove worktree");
    assert!(!checkout.exists());

    // The quarantine marker forbids ref updates of any git child that sees it,
    // and creating a branch is a ref update.
    assert!(
        std::env::var_os(QUARANTINE_VAR).is_some_and(|dir| Path::new(&dir).is_dir()),
        "the child starts with the quarantine marker set to an existing directory"
    );
    let quarantined = env.root().join("quarantined");
    worktree_add_new(&repo, &quarantined, "quarantined-feature", "main")
        .expect("add worktree despite the quarantine marker");
    assert_eq!(branch_exists(&repo, "quarantined-feature"), Ok(true));
    worktree_remove(&repo, &quarantined).expect("remove quarantined worktree");

    // A replace ref substitutes `HEAD` with the first commit; the daemon applies
    // it like the fixture does.
    let first = fixture_git(&env, &repo, &["rev-list", "--max-parents=0", "HEAD"]);
    fixture_git(&env, &repo, &["replace", &head, &first]);
    let expected = fixture_git(&env, &repo, &["rev-list", "--count", "HEAD"]);
    assert_eq!(expected, "1");
    assert_eq!(
        crate::project::detect::git(&repo, &["rev-list", "--count", "HEAD"]).as_deref(),
        Some(expected.as_str()),
        "the daemon applies the repository's replace refs"
    );
}

#[test]
fn ambient_git_variables_do_not_redirect_daemon_git() {
    let env = TestEnv::new().expect("hermetic test environment");
    let repo = env.root().join("repo");
    let quarantine = env.root().join("quarantine");
    std::fs::create_dir(&quarantine).expect("create quarantine directory");
    let output = env
        .command(std::env::current_exe().expect("test executable"))
        .args([
            "--ignored",
            "--exact",
            "worktree::git_env_tests::daemon_git_under_ambient_git_variables",
        ])
        .envs(AMBIENT_GIT_VARS)
        .envs(ISOLATED_GIT_CONFIG)
        .env(SHALLOW_FILE_VAR, env.root().join("shallow"))
        .env(GRAFT_FILE_VAR, env.root().join("grafts"))
        .env("GIT_CEILING_DIRECTORIES", &repo)
        .env(QUARANTINE_VAR, &quarantine)
        .env(REPO_VAR, &repo)
        .output()
        .expect("run scenario child");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Variable naming the hooks directory the parent configured as
/// `core.hooksPath` through command-scoped configuration.
const HOOKS_DIR_VAR: &str = "POHUNEK_TEST_GIT_HOOKS_DIR";

/// File a `post-checkout` hook in the hooks directory creates when it runs.
const HOOK_MARKER: &str = "ran";

/// Writes an executable `post-checkout` hook that creates [`HOOK_MARKER`] next to
/// itself and returns its directory.
fn write_marker_hook(root: &Path, name: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let hooks = root.join(name);
    std::fs::create_dir(&hooks).expect("create hooks directory");
    let hook = hooks.join("post-checkout");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\n: > '{}'\n", hooks.join(HOOK_MARKER).display()),
    )
    .expect("write hook");
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))
        .expect("make hook executable");
    hooks
}

/// Child half of the command-scoped configuration tests: the environment holds
/// a `core.hooksPath` override for [`HOOKS_DIR_VAR`], and a worktree the daemon
/// adds must not run the hook.
fn daemon_worktree_add_ignores_command_scoped_hooks_path() {
    let env = TestEnv::new().expect("hermetic test environment");
    let repo = env.root().join("repo");
    let hooks = PathBuf::from(std::env::var_os(HOOKS_DIR_VAR).expect("hooks directory"));
    let marker = hooks.join(HOOK_MARKER);
    std::fs::create_dir(&repo).expect("create repository directory");
    fixture_git(&env, &repo, &["init", "-q", "-b", "main"]);
    fixture_git(&env, &repo, &["config", "user.email", "test@example.com"]);
    fixture_git(&env, &repo, &["config", "user.name", "Test"]);
    fixture_git(&env, &repo, &["config", "commit.gpgsign", "false"]);
    fixture_git(
        &env,
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "init"],
    );

    // Control: a git child that inherits the environment runs the hook, so the
    // override is effective for a Git that honors it.
    let control = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "add", "-q", "--detach"])
        .arg(env.root().join("control"))
        .output()
        .expect("run control git");
    assert!(
        control.status.success(),
        "{}",
        String::from_utf8_lossy(&control.stderr)
    );
    assert!(marker.is_file(), "the inherited override runs the hook");
    std::fs::remove_file(&marker).expect("remove marker");

    let checkout = env.root().join("checkout");
    worktree_add_new(&repo, &checkout, "feature", "main").expect("add worktree");
    assert!(checkout.is_dir());
    assert!(
        !marker.exists(),
        "the daemon's worktree add ran a hook from command-scoped configuration"
    );
}

/// Child half of [`command_scoped_config_parameters_do_not_reach_daemon_git`].
#[test]
#[ignore = "child process of command_scoped_config_parameters_do_not_reach_daemon_git"]
fn daemon_git_under_config_parameters() {
    daemon_worktree_add_ignores_command_scoped_hooks_path();
}

/// Child half of [`command_scoped_config_entries_do_not_reach_daemon_git`].
#[test]
#[ignore = "child process of command_scoped_config_entries_do_not_reach_daemon_git"]
fn daemon_git_under_config_entries() {
    daemon_worktree_add_ignores_command_scoped_hooks_path();
}

/// Runs the ignored child test `name` with `configure` applied to its
/// environment and asserts that it passes.
fn run_hooks_path_child(name: &str, configure: impl FnOnce(&mut std::process::Command, &Path)) {
    let env = TestEnv::new().expect("hermetic test environment");
    let hooks = write_marker_hook(env.root(), "hooks");
    let mut command = env.command(std::env::current_exe().expect("test executable"));
    command
        .args(["--ignored", "--exact", name])
        .envs(ISOLATED_GIT_CONFIG)
        .env(HOOKS_DIR_VAR, &hooks);
    configure(&mut command, &hooks);
    let output = command.output().expect("run scenario child");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `git -c core.hooksPath=<dir>` exports `GIT_CONFIG_PARAMETERS` to a hook.
#[test]
fn command_scoped_config_parameters_do_not_reach_daemon_git() {
    run_hooks_path_child(
        "worktree::git_env_tests::daemon_git_under_config_parameters",
        |command, hooks| {
            command.env(
                "GIT_CONFIG_PARAMETERS",
                format!("'core.hooksPath'='{}'", hooks.display()),
            );
        },
    );
}

/// `GIT_CONFIG_COUNT` with `GIT_CONFIG_KEY_<n>` and `GIT_CONFIG_VALUE_<n>`
/// carries the same override.
#[test]
fn command_scoped_config_entries_do_not_reach_daemon_git() {
    run_hooks_path_child(
        "worktree::git_env_tests::daemon_git_under_config_entries",
        |command, hooks| {
            command
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "core.hooksPath")
                .env("GIT_CONFIG_VALUE_0", hooks);
        },
    );
}

/// Variable naming the directory the parent set as `GIT_EXEC_PATH`, as Git does
/// for its hooks; it holds a `git-remote-https` helper that records its run.
const EXEC_DIR_VAR: &str = "POHUNEK_TEST_GIT_EXEC_DIR";

/// File the `git-remote-https` helper creates when Git runs it.
const HELPER_MARKER: &str = "helper-ran";

/// Writes an executable `git-remote-https` that creates [`HELPER_MARKER`] next to
/// itself and fails, and returns its directory.
fn write_marker_remote_helper(root: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let exec = root.join("exec");
    std::fs::create_dir(&exec).expect("create exec directory");
    let helper = exec.join("git-remote-https");
    std::fs::write(
        &helper,
        format!(
            "#!/bin/sh\n: > '{}'\nexit 1\n",
            exec.join(HELPER_MARKER).display()
        ),
    )
    .expect("write helper");
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755))
        .expect("make helper executable");
    exec
}

/// Child half of [`exec_path_does_not_reach_daemon_git`]: the daemon's fetch from
/// an HTTPS remote must resolve `git-remote-https` without the inherited
/// `GIT_EXEC_PATH`.
#[test]
#[ignore = "child process of exec_path_does_not_reach_daemon_git"]
fn daemon_fetch_under_exec_path() {
    let env = TestEnv::new().expect("hermetic test environment");
    let repo = env.root().join("repo");
    let exec = PathBuf::from(std::env::var_os(EXEC_DIR_VAR).expect("exec directory"));
    let marker = exec.join(HELPER_MARKER);
    std::fs::create_dir(&repo).expect("create repository directory");
    fixture_git(&env, &repo, &["init", "-q", "-b", "main"]);
    // Nothing listens on the discard port, so a fetch that reaches the real
    // helper fails fast with a refused connection.
    fixture_git(
        &env,
        &repo,
        &["remote", "add", "origin", "https://127.0.0.1:9/pohunek.git"],
    );

    // Control: a git child that inherits the environment runs the planted
    // helper, so the variable is effective for this Git.
    let control = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["fetch", "--no-tags", "origin", "main"])
        .output()
        .expect("run control git");
    assert!(!control.status.success());
    assert!(marker.is_file(), "the inherited exec path runs the helper");
    std::fs::remove_file(&marker).expect("remove marker");

    assert!(fetch_origin(&repo, "main").is_err());
    assert!(
        !marker.exists(),
        "the daemon's fetch ran a helper from the inherited GIT_EXEC_PATH"
    );
}

/// Git exports `GIT_EXEC_PATH` to hooks; a daemon started from one must not look
/// for helpers in that directory.
#[test]
fn exec_path_does_not_reach_daemon_git() {
    let env = TestEnv::new().expect("hermetic test environment");
    let exec = write_marker_remote_helper(env.root());
    let output = env
        .command(std::env::current_exe().expect("test executable"))
        .args([
            "--ignored",
            "--exact",
            "worktree::git_env_tests::daemon_fetch_under_exec_path",
        ])
        .envs(ISOLATED_GIT_CONFIG)
        .env("GIT_EXEC_PATH", &exec)
        .env(EXEC_DIR_VAR, &exec)
        .output()
        .expect("run scenario child");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
