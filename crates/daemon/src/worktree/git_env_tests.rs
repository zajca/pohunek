//! Daemon `git` children ignore repository-location variables of the
//! environment they inherit, as in a daemon started from a git hook.
//!
//! The variables are set on a child process, because mutating this process's
//! environment would race every other test.

use std::path::{Path, PathBuf};

use pohunek_test_support::env::TestEnv;

use super::{branch_exists, worktree_add_new, worktree_remove};

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
