//! Unit tests for the worktree binder, store, slug, ownership, and the three
//! non-fatal warning paths. Each test that needs a repository builds a real,
//! throwaway git repo under the system temp dir (git is required, as it is for
//! the daemon at runtime).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use protocol::SessionWarningKind;

use super::{
    branch_slug, hook_env, is_valid_worktree, HookContext, HookEvent, WorktreeManager,
    WorktreeRequest,
};
use crate::store::{Store, WorktreeStatus};

/// Generous setup-script timeout for tests whose script finishes promptly; long
/// enough that it never trips on a slow CI box.
const TEST_SETUP_TIMEOUT: Duration = Duration::from_secs(30);

fn unique_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("pohunek-wt-{tag}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn git_in(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Initialize a git repo on branch `main` with one commit.
fn init_repo(tag: &str) -> PathBuf {
    let dir = unique_dir(tag);
    let init = Command::new("git")
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
        .arg(&dir)
        .output()
        .expect("git init");
    assert!(
        init.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    git_in(&dir, &["config", "user.email", "test@example.com"]);
    git_in(&dir, &["config", "user.name", "Test"]);
    git_in(&dir, &["config", "commit.gpgsign", "false"]);
    fs::write(dir.join("README.md"), "init\n").expect("write README");
    git_in(&dir, &["add", "."]);
    git_in(&dir, &["commit", "-q", "-m", "init"]);
    dir
}

/// Initialize a **bare** repo (no working tree) carrying a commit and HEAD, via a
/// `--bare` clone of a normal repo so `git worktree add` off it has a base ref.
fn init_bare_repo(tag: &str) -> PathBuf {
    let source = init_repo(&format!("{tag}-src"));
    let bare = unique_dir(&format!("{tag}-bare")).join("repo.git");
    let clone = Command::new("git")
        .args(["clone", "--bare", "-q"])
        .arg(&source)
        .arg(&bare)
        .output()
        .expect("git clone --bare");
    assert!(
        clone.status.success(),
        "git clone --bare failed: {}",
        String::from_utf8_lossy(&clone.stderr)
    );
    bare
}

fn manager(tag: &str) -> WorktreeManager {
    manager_with_timeout(tag, TEST_SETUP_TIMEOUT)
}

#[test]
fn session_hook_events_have_stable_env_tokens() {
    assert_eq!(HookEvent::SessionStart.as_env(), "session-start");
    assert_eq!(HookEvent::SessionStop.as_env(), "session-stop");
    assert_eq!(HookEvent::AgentState.as_env(), "agent-state");
}

#[test]
fn hook_env_includes_session_stop_reason_and_agent_activity() {
    let ctx = HookContext {
        session_id: "s-7".to_owned(),
        project_id: None,
        agent: "codex".to_owned(),
        repo: None,
        worktree: None,
        branch: None,
        base_branch: None,
        stop_reason: Some("failed"),
        activity: Some("blocked"),
    };

    let env = hook_env(HookEvent::AgentState, &ctx);
    let lookup = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());

    assert_eq!(lookup("POHUNEK_STOP_REASON"), Some("failed"));
    assert_eq!(lookup("POHUNEK_ACTIVITY"), Some("blocked"));
}

fn manager_with_timeout(tag: &str, setup_timeout: Duration) -> WorktreeManager {
    let root = unique_dir(&format!("{tag}-root"));
    let store = unique_dir(&format!("{tag}-store")).join("metadata.jsonl");
    WorktreeManager::new(root, Arc::new(Store::new(store)), setup_timeout, None)
}

/// A manager with a host-global hook layer rooted at `config_dir`.
fn manager_with_config_dir(tag: &str, config_dir: PathBuf) -> WorktreeManager {
    let root = unique_dir(&format!("{tag}-root"));
    let store = unique_dir(&format!("{tag}-store")).join("metadata.jsonl");
    WorktreeManager::new(
        root,
        Arc::new(Store::new(store)),
        TEST_SETUP_TIMEOUT,
        Some(config_dir),
    )
}

/// Run git in `dir` and return trimmed stdout (asserting success).
fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn request(session: &str, repo: &Path, branch: &str) -> WorktreeRequest {
    WorktreeRequest {
        session_id: session.to_owned(),
        repo: repo.to_path_buf(),
        branch: branch.to_owned(),
        base_branch: None,
        project_id: None,
        agent: "claude".to_owned(),
    }
}

// --- slug -----------------------------------------------------------------

#[test]
fn branch_slug_matches_reference_cases() {
    // Ported verbatim from Kandev slug_test.go TestSanitizeBranchSlug.
    let cases = [
        ("main", "main"),
        ("feature/foo", "feature-foo"),
        ("feature/foo-bar", "feature-foo-bar"),
        ("release/v1.2.3", "release-v1.2.3"),
        ("user/jane/wip", "user-jane-wip"),
        ("-leading", "leading"),
        ("trailing-", "trailing"),
        ("slash/", "slash"),
        ("a//b", "a-b"),
        ("weird:name space", "weird-name-space"),
        ("!@#$%", ""),
        ("", ""),
        (".hidden", "hidden"),
        ("修复登录问题", ""),
        ("foo--bar", "foo-bar"),
    ];
    for (input, want) in cases {
        assert_eq!(branch_slug(input), want, "branch_slug({input:?})");
    }
}

#[test]
fn worktree_path_disambiguates_two_branches_of_one_session() {
    let mgr = manager("slug-path");
    let repo = PathBuf::from("/workspace/project");
    let a = mgr
        .worktree_path("s-1", &repo, "feature/a")
        .expect("path a");
    let b = mgr
        .worktree_path("s-1", &repo, "feature/b")
        .expect("path b");
    assert_ne!(a, b, "two branches must not collapse to one path");
    assert!(a.to_string_lossy().ends_with("s-1-project-feature-a"));
    assert!(b.to_string_lossy().ends_with("s-1-project-feature-b"));
}

#[test]
fn worktree_path_rejects_unsluggable_branch() {
    let mgr = manager("slug-empty");
    let err = mgr
        .worktree_path("s-1", Path::new("/workspace/project"), "修复")
        .expect_err("all-non-ascii branch has no slug");
    assert_eq!(err.code, "invalid_branch_slug");
}

// --- bind: create / reuse / ownership -------------------------------------

#[test]
fn bind_creates_a_valid_worktree_and_records_a_binding() {
    let mgr = manager("create");
    let repo = init_repo("create-repo");
    let bound = mgr
        .bind(&request("s-1", &repo, "feat/x"))
        .expect("bind worktree");

    assert!(!bound.reused);
    assert!(
        bound.warnings.is_empty(),
        "clean create: {:?}",
        bound.warnings
    );
    assert!(
        is_valid_worktree(&bound.path),
        "bound path must be a git worktree"
    );
    assert_eq!(bound.base_branch, "main");

    let binding = mgr
        .store()
        .find_worktree("s-1", &bound.repository, &branch_slug("feat/x"))
        .expect("load store")
        .expect("binding recorded");
    assert_eq!(binding.path, bound.path);
    assert_eq!(binding.branch, "feat/x");
    assert_eq!(binding.status, WorktreeStatus::Active);
}

#[test]
fn bind_creates_a_worktree_on_a_bare_repo() {
    // A bare repo has no working tree, so an in-place session is refused and the
    // caller is steered to --branch. That steer is only valid if the manager
    // accepts a bare repo as the worktree source: `git worktree add` works on bare
    // repos, so `is_git_repo` (the bind gate) must accept them too.
    let mgr = manager("bare-bind");
    let bare = init_bare_repo("bare-bind");
    let bound = mgr
        .bind(&request("s-1", &bare, "feat/x"))
        .expect("a worktree can be bound off a bare repo");
    assert!(
        is_valid_worktree(&bound.path),
        "a real worktree is created off the bare repo"
    );
    assert_eq!(bound.branch, "feat/x");
    assert_eq!(
        bound.base_branch, "main",
        "branched off the bare repo's HEAD"
    );
}

#[test]
fn bind_reuses_an_owned_valid_worktree() {
    let mgr = manager("reuse");
    let repo = init_repo("reuse-repo");
    let first = mgr
        .bind(&request("s-1", &repo, "feat/x"))
        .expect("first bind");
    assert!(!first.reused);

    // Re-binding the same (session, repo, branch) must reuse, not re-run
    // `git worktree add -b` (which would fail because the branch now exists).
    let second = mgr
        .bind(&request("s-1", &repo, "feat/x"))
        .expect("second bind reuses");
    assert!(second.reused, "second bind should reuse the owned worktree");
    assert_eq!(second.path, first.path);
    assert!(is_valid_worktree(&second.path));
}

#[test]
fn two_branches_of_one_repo_bind_two_distinct_trees() {
    let mgr = manager("two-branches");
    let repo = init_repo("two-branches-repo");
    let a = mgr.bind(&request("s-1", &repo, "feat/a")).expect("bind a");
    let b = mgr.bind(&request("s-1", &repo, "feat/b")).expect("bind b");

    assert_ne!(a.path, b.path, "two branches must not share a tree");
    assert!(is_valid_worktree(&a.path));
    assert!(is_valid_worktree(&b.path));
}

#[test]
fn bind_refuses_a_foreign_directory_at_the_target_path() {
    let mgr = manager("foreign");
    let repo = init_repo("foreign-repo");
    // Pre-create a directory at the exact target path WITHOUT a binding: the
    // daemon does not own it and must refuse to adopt or clobber it.
    let target = mgr
        .worktree_path("s-1", &super::canonical_or_original(&repo), "feat/x")
        .expect("target path");
    fs::create_dir_all(&target).expect("create foreign dir");

    let err = mgr
        .bind(&request("s-1", &repo, "feat/x"))
        .expect_err("foreign tree must be refused");
    assert_eq!(err.code, "worktree_path_conflict");
}

#[test]
fn bind_recreates_when_owned_directory_was_lost() {
    let mgr = manager("recreate");
    let repo = init_repo("recreate-repo");
    let first = mgr
        .bind(&request("s-1", &repo, "feat/x"))
        .expect("first bind");

    // Simulate the worktree directory being lost while the binding remains.
    fs::remove_dir_all(&first.path).expect("remove worktree dir");
    assert!(!is_valid_worktree(&first.path));

    let again = mgr
        .bind(&request("s-1", &repo, "feat/x"))
        .expect("recreate owned worktree");
    assert!(!again.reused, "a lost directory is recreated, not reused");
    assert_eq!(again.path, first.path);
    assert!(is_valid_worktree(&again.path));
}

#[test]
fn bind_rejects_a_non_git_repository() {
    let mgr = manager("nonrepo");
    let not_a_repo = unique_dir("nonrepo-dir");
    let err = mgr
        .bind(&request("s-1", &not_a_repo, "feat/x"))
        .expect_err("non-git repo must error");
    assert_eq!(err.code, "not_a_git_repo");
}

#[test]
fn bind_rejects_flag_injecting_branch_and_base() {
    let mgr = manager("flag-inject");
    let repo = init_repo("flag-inject-repo");

    // A branch beginning with '-' must be refused before any git invocation so
    // it cannot be parsed as a git flag.
    let err = mgr
        .bind(&request("s-1", &repo, "--upload-pack=evil"))
        .expect_err("dash-leading branch rejected");
    assert_eq!(err.code, "invalid_branch");

    // Same guard for a dash-leading base branch.
    let mut req = request("s-2", &repo, "feat/x");
    req.base_branch = Some("--exec=evil".to_owned());
    let err = mgr.bind(&req).expect_err("dash-leading base rejected");
    assert_eq!(err.code, "invalid_branch");
}

#[test]
fn bind_rejects_flag_injecting_default_branch_from_crafted_head() {
    // A malicious repository can point HEAD at a dash-leading ref so the
    // resolved default branch smuggles a git flag (e.g. `--upload-pack=cmd`)
    // into `git fetch`. The default branch must be validated like any other ref.
    let mgr = manager("crafted-head");
    let repo = init_repo("crafted-head-repo");
    // Configure an origin so the fetch path would be reached if validation
    // failed to catch the crafted HEAD.
    let bogus = unique_dir("crafted-head-origin").join("missing.git");
    git_in(
        &repo,
        &["remote", "add", "origin", &bogus.to_string_lossy()],
    );
    // Craft HEAD to a dash-leading ref directly (a real attacker controls the
    // repo's .git contents).
    fs::write(
        repo.join(".git/HEAD"),
        "ref: refs/heads/--upload-pack=evil\n",
    )
    .expect("write crafted HEAD");

    // Bind WITHOUT an explicit base so the crafted default branch is resolved.
    let err = mgr
        .bind(&request("s-1", &repo, "feat/x"))
        .expect_err("crafted default branch must be rejected");
    assert_eq!(err.code, "invalid_branch", "got: {err:?}");
    // No worktree was created — the malicious ref never reached a git sink.
    let target = mgr
        .worktree_path("s-1", &super::canonical_or_original(&repo), "feat/x")
        .expect("target path");
    assert!(!target.exists(), "no worktree should have been created");
}

#[test]
fn detached_head_binds_with_explicit_base_but_fails_without() {
    let mgr = manager("detached");
    let repo = init_repo("detached-repo");
    // Detach HEAD at the current commit.
    git_in(&repo, &["checkout", "--detach", "--quiet"]);

    // With an explicit, existing base branch, binding succeeds — the default
    // branch is never consulted, so detached HEAD is not a problem.
    let with_base = {
        let mut req = request("s-1", &repo, "feat/x");
        req.base_branch = Some("main".to_owned());
        mgr.bind(&req)
            .expect("bind with explicit base on detached HEAD")
    };
    assert!(is_valid_worktree(&with_base.path));
    assert_eq!(with_base.base_branch, "main");

    // Without a base, the default branch cannot be resolved on detached HEAD.
    let err = mgr
        .bind(&request("s-2", &repo, "feat/y"))
        .expect_err("detached HEAD without a base must error clearly");
    assert_eq!(err.code, "invalid_base_branch");
    assert!(
        err.msg.contains("detached HEAD"),
        "error should mention detached HEAD: {}",
        err.msg
    );
}

#[test]
fn successful_fetch_starts_the_worktree_from_the_fetched_commit() {
    // Upstream gains a commit AFTER the downstream clone, so the downstream
    // local base ref is stale. A successful fetch must start the new branch
    // from the fetched tip, not the stale local ref.
    let upstream = init_repo("fetch-upstream");
    let downstream = unique_dir("fetch-downstream-parent").join("clone");
    let clone = Command::new("git")
        .args(["clone", "-q"])
        .arg(&upstream)
        .arg(&downstream)
        .output()
        .expect("git clone");
    assert!(
        clone.status.success(),
        "git clone failed: {}",
        String::from_utf8_lossy(&clone.stderr)
    );
    git_in(&downstream, &["config", "user.email", "test@example.com"]);
    git_in(&downstream, &["config", "user.name", "Test"]);
    git_in(&downstream, &["config", "commit.gpgsign", "false"]);

    // Advance upstream main beyond the clone.
    std::fs::write(upstream.join("NEW.md"), "v2\n").expect("write upstream file");
    git_in(&upstream, &["add", "."]);
    git_in(&upstream, &["commit", "-q", "-m", "v2"]);
    let upstream_tip = git_stdout(&upstream, &["rev-parse", "HEAD"]);
    let stale_local = git_stdout(&downstream, &["rev-parse", "refs/heads/main"]);
    assert_ne!(
        upstream_tip, stale_local,
        "local must be stale before fetch"
    );

    let mgr = manager("fetch-success");
    let mut req = request("s-1", &downstream, "feat/x");
    req.base_branch = Some("main".to_owned());
    let bound = mgr.bind(&req).expect("bind fetches and binds");

    assert!(
        bound.warnings.is_empty(),
        "successful fetch must not warn: {:?}",
        bound.warnings
    );
    let worktree_tip = git_stdout(&bound.path, &["rev-parse", "HEAD"]);
    assert_eq!(
        worktree_tip, upstream_tip,
        "worktree must start from the fetched tip, not the stale local ref"
    );
}

#[test]
fn second_session_on_same_branch_gets_a_clear_in_use_error() {
    let mgr = manager("same-branch");
    let repo = init_repo("same-branch-repo");
    let first = mgr
        .bind(&request("s-1", &repo, "feat/shared"))
        .expect("first bind");
    assert!(is_valid_worktree(&first.path));

    // A different session requesting the SAME branch cannot get a second
    // worktree (git allows one worktree per branch); the error must be clear.
    let err = mgr
        .bind(&request("s-2", &repo, "feat/shared"))
        .expect_err("same branch in a second session must fail clearly");
    assert_eq!(err.code, "worktree_branch_in_use", "got: {err:?}");
}

// --- non-fatal warning paths ----------------------------------------------

#[test]
fn base_branch_fallback_keeps_the_worktree_with_a_warning() {
    let mgr = manager("base-fallback");
    let repo = init_repo("base-fallback-repo");
    let mut req = request("s-1", &repo, "feat/x");
    req.base_branch = Some("release/does-not-exist".to_owned());

    let bound = mgr.bind(&req).expect("bind falls back, does not abort");
    assert!(
        is_valid_worktree(&bound.path),
        "worktree must survive fallback"
    );
    assert_eq!(bound.base_branch, "main", "fell back to the default branch");
    let warning = bound
        .warnings
        .iter()
        .find(|w| w.kind == SessionWarningKind::BaseBranchFallback)
        .expect("base-branch fallback warning present");
    assert!(warning.message.contains("release/does-not-exist"));
    assert!(warning.message.contains("main"));
}

#[test]
fn fetch_failure_keeps_the_worktree_with_a_warning() {
    let mgr = manager("fetch-warn");
    let repo = init_repo("fetch-warn-repo");
    // Configure an origin remote that cannot be fetched from: `git fetch origin
    // main` fails, but the local `main` exists, so binding falls back to it.
    let bogus = unique_dir("fetch-warn-bogus").join("missing.git");
    git_in(
        &repo,
        &["remote", "add", "origin", &bogus.to_string_lossy()],
    );

    let bound = mgr
        .bind(&request("s-1", &repo, "feat/x"))
        .expect("bind falls back when fetch fails");
    assert!(
        is_valid_worktree(&bound.path),
        "worktree must survive fetch failure"
    );
    assert!(
        bound
            .warnings
            .iter()
            .any(|w| w.kind == SessionWarningKind::Fetch),
        "fetch warning present: {:?}",
        bound.warnings
    );
}

#[test]
fn clean_repo_without_origin_produces_no_fetch_warning() {
    let mgr = manager("no-origin");
    let repo = init_repo("no-origin-repo");
    let bound = mgr.bind(&request("s-1", &repo, "feat/x")).expect("bind");
    assert!(
        !bound
            .warnings
            .iter()
            .any(|w| w.kind == SessionWarningKind::Fetch),
        "no origin means nothing to fetch and no warning: {:?}",
        bound.warnings
    );
}

#[test]
fn failing_setup_script_keeps_the_worktree_with_a_warning() {
    let mgr = manager("setup-warn");
    let repo = init_repo("setup-warn-repo");
    // Commit a setup script on main that exits non-zero; the worktree (created
    // from main) inherits it.
    fs::create_dir_all(repo.join(".pohunek")).expect("create .pohunek");
    fs::write(
        repo.join(".pohunek/setup"),
        "#!/bin/sh\necho 'boom' >&2\nexit 3\n",
    )
    .expect("write setup script");
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-q", "-m", "add failing setup"]);

    let bound = mgr
        .bind(&request("s-1", &repo, "feat/x"))
        .expect("bind keeps worktree despite setup failure");
    assert!(
        is_valid_worktree(&bound.path),
        "worktree must survive setup failure"
    );
    let warning = bound
        .warnings
        .iter()
        .find(|w| w.kind == SessionWarningKind::Hook)
        .expect("setup-script warning present");
    assert!(warning.detail.is_some(), "setup warning carries detail");
}

#[test]
fn successful_setup_script_produces_no_warning() {
    let mgr = manager("setup-ok");
    let repo = init_repo("setup-ok-repo");
    fs::create_dir_all(repo.join(".pohunek")).expect("create .pohunek");
    fs::write(repo.join(".pohunek/setup"), "#!/bin/sh\nexit 0\n").expect("write setup");
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-q", "-m", "add ok setup"]);

    let bound = mgr.bind(&request("s-1", &repo, "feat/x")).expect("bind");
    assert!(
        bound.warnings.is_empty(),
        "a passing setup script must not warn: {:?}",
        bound.warnings
    );
}

#[test]
fn failing_setup_script_warning_detail_excludes_script_stderr() {
    // The event log claims to never hold a secret, but a setup script's stderr is
    // arbitrary process output (e.g. `echo $TOKEN`). The warning detail — which
    // rides into the event log — must therefore carry only the exit status, not
    // the script's stderr, even though the script is the user's own committed file.
    let mgr = manager("setup-secret");
    let repo = init_repo("setup-secret-repo");
    let secret = "SUPER_SECRET_TOKEN_abc123";
    fs::create_dir_all(repo.join(".pohunek")).expect("create .pohunek");
    fs::write(
        repo.join(".pohunek/setup"),
        format!("#!/bin/sh\necho '{secret}' >&2\nexit 7\n"),
    )
    .expect("write setup script");
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-q", "-m", "add leaking setup"]);

    let bound = mgr
        .bind(&request("s-1", &repo, "feat/x"))
        .expect("bind keeps worktree");
    let warning = bound
        .warnings
        .iter()
        .find(|w| w.kind == SessionWarningKind::Hook)
        .expect("setup-script warning present");
    let detail = warning.detail.as_deref().unwrap_or_default();
    assert!(
        !detail.contains(secret),
        "setup-script stderr must not leak into the warning detail (event log): {detail:?}"
    );
    assert!(
        !warning.message.contains(secret),
        "setup-script stderr must not leak into the warning message: {:?}",
        warning.message
    );
}

#[test]
fn hanging_setup_script_is_terminated_with_its_forked_children() {
    // A script that never exits must not wedge `bind`/`session.new`, and killing
    // it must take its forked children with it — killing only the direct shell
    // would leave them as runaway processes. The script forks a `sleep`, records
    // its pid, then blocks; after the timeout the whole process group is killed,
    // so that child must be gone too.
    let mgr = manager_with_timeout("setup-timeout", Duration::from_secs(1));
    let repo = init_repo("setup-timeout-repo");
    fs::create_dir_all(repo.join(".pohunek")).expect("create .pohunek");
    // `sleep 30` is bounded so a regression that leaves a runaway self-terminates
    // rather than lingering for the box's lifetime. No shell exec-optimization
    // (the shell stays a real parent of a real child) thanks to the trailing
    // `wait`. The pid is written into the (kept) worktree for the test to probe.
    fs::write(
        repo.join(".pohunek/setup"),
        "#!/bin/sh\nsleep 30 &\necho \"$!\" > setup-child.pid\nwait\n",
    )
    .expect("write setup");
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-q", "-m", "add hanging setup"]);

    let started = Instant::now();
    let bound = mgr
        .bind(&request("s-1", &repo, "feat/x"))
        .expect("bind returns despite a hanging setup script");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(20),
        "bind must return shortly after the timeout, took {elapsed:?}"
    );
    assert!(
        is_valid_worktree(&bound.path),
        "worktree kept after a setup timeout"
    );

    let warning = bound
        .warnings
        .iter()
        .find(|w| w.kind == SessionWarningKind::Hook)
        .expect("setup-script timeout warning present");
    let detail = warning.detail.as_deref().unwrap_or_default();
    assert!(
        detail.contains("did not finish") || detail.contains("terminated"),
        "timeout warning detail should explain the termination: {detail:?}"
    );

    // The forked child must have been killed with the process group — a regression
    // that signalled only the direct shell would leave it alive as a runaway.
    let child_pid = read_setup_child_pid(&bound.path);
    assert!(
        wait_until_process_gone(child_pid, Duration::from_secs(10)),
        "setup script's forked child (pid {child_pid}) survived the timeout kill as a runaway"
    );
}

/// Read the pid the timed-out setup script recorded for its forked child, polling
/// briefly for the file to settle on disk.
fn read_setup_child_pid(worktree: &Path) -> i32 {
    let pid_file = worktree.join("setup-child.pid");
    for _ in 0..50 {
        if let Ok(contents) = fs::read_to_string(&pid_file) {
            if let Ok(pid) = contents.trim().parse::<i32>() {
                return pid;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "setup script never recorded its child pid at {}",
        pid_file.display()
    );
}

/// Whether `pid` has disappeared within `budget`. Uses `kill -0`, which succeeds
/// while the pid exists (briefly true for a zombie awaiting reaping) and fails
/// once it is gone, so a short poll absorbs the reap window without flaking.
fn wait_until_process_gone(pid: i32, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        let alive = Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !alive {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
#[test]
fn binding_persist_failure_rolls_back_the_worktree() {
    use std::os::unix::fs::PermissionsExt;

    // bind() persists the worktree binding only after the git worktree and its
    // branch checkout already exist. If that persist fails it must roll the
    // checkout back (the sibling of the create() orphan fix), or the branch stays
    // checked out and blocks the next session.new with `worktree_branch_in_use`.
    let root = unique_dir("persist-fail-root");
    let store_dir = unique_dir("persist-fail-store");
    let store_path = store_dir.join("metadata.jsonl");
    let mgr = WorktreeManager::new(
        root,
        Arc::new(Store::new(store_path)),
        TEST_SETUP_TIMEOUT,
        None,
    );
    let repo = init_repo("persist-fail-repo");

    // Force the persist to fail by making the store directory read-only: the store
    // writes via a temp file + atomic rename inside it, which then fails with
    // EACCES. Root bypasses directory permission bits, so skip there.
    fs::set_permissions(&store_dir, fs::Permissions::from_mode(0o500))
        .expect("make store dir read-only");
    let probe = store_dir.join(".probe");
    if fs::write(&probe, b"x").is_ok() {
        let _ = fs::remove_file(&probe);
        fs::set_permissions(&store_dir, fs::Permissions::from_mode(0o755)).ok();
        eprintln!(
            "skipping binding_persist_failure_rolls_back_the_worktree: perms not enforced (root?)"
        );
        return;
    }

    let result = mgr.bind(&request("s-1", &repo, "feat/x"));

    // Restore perms first so the temp dir is cleanable regardless of the outcome.
    fs::set_permissions(&store_dir, fs::Permissions::from_mode(0o755))
        .expect("restore store dir perms");

    let err = result.expect_err("bind must fail when the binding cannot be persisted");
    assert_eq!(err.code, "worktree_store_error", "got: {err:?}");

    // The worktree and its branch checkout must have been rolled back, freeing the
    // branch for a retry rather than orphaning it.
    let listing = git_stdout(&repo, &["worktree", "list", "--porcelain"]);
    assert!(
        !listing.contains("feat/x"),
        "binding-persist failure must roll back the checkout: {listing}"
    );
}

// --- cleanup ownership ----------------------------------------------------

#[test]
fn cleanup_session_removes_only_owned_worktrees() {
    let mgr = manager("cleanup");
    let repo = init_repo("cleanup-repo");
    let bound = mgr.bind(&request("s-1", &repo, "feat/x")).expect("bind");
    assert!(bound.path.exists());

    // Unknown session: nothing owned, nothing removed.
    let none = mgr
        .cleanup_session("s-unknown", &mut Vec::new())
        .expect("cleanup unknown");
    assert_eq!(none, 0);
    assert!(
        bound.path.exists(),
        "an unowned session must not touch our tree"
    );

    // Owned session: the tree and its binding are removed.
    let removed = mgr
        .cleanup_session("s-1", &mut Vec::new())
        .expect("cleanup owned");
    assert_eq!(removed, 1);
    assert!(!bound.path.exists(), "owned worktree directory removed");
    assert!(
        mgr.store()
            .find_worktree("s-1", &bound.repository, &branch_slug("feat/x"))
            .expect("load store")
            .is_none(),
        "binding dropped after cleanup"
    );
}

#[test]
fn cleanup_project_removes_only_the_projects_owned_worktrees() {
    let mgr = manager("cleanup-project");
    let repo = init_repo("cleanup-project-repo");

    // Two worktrees owned by project p-1, one by p-2 (all in one repo).
    let bind_owned = |session: &str, branch: &str, project: &str| {
        let mut req = request(session, &repo, branch);
        req.project_id = Some(project.to_owned());
        mgr.bind(&req).expect("bind")
    };
    let a = bind_owned("s-1", "feat/a", "p-1");
    let b = bind_owned("s-2", "feat/b", "p-1");
    let c = bind_owned("s-3", "feat/c", "p-2");
    assert!(a.path.exists() && b.path.exists() && c.path.exists());
    let no_skip = HashSet::new();

    // Pruning p-1 (nothing skipped) removes both of its worktrees, never p-2's.
    let prune = mgr
        .cleanup_project("p-1", &no_skip, &mut Vec::new())
        .expect("cleanup p-1");
    assert_eq!(prune.removed, 2);
    assert!(prune.skipped.is_empty());
    assert!(!a.path.exists(), "p-1 worktree a removed");
    assert!(!b.path.exists(), "p-1 worktree b removed");
    assert!(c.path.exists(), "p-2 worktree must be untouched");

    // Only p-2's binding remains in the store.
    let remaining = mgr.store().load_worktrees().expect("load store");
    assert_eq!(
        remaining.len(),
        1,
        "only p-2 binding remains: {remaining:?}"
    );
    assert_eq!(remaining[0].project_id.as_deref(), Some("p-2"));

    // Skipping p-2's worktree (a live session is using it) leaves it AND its
    // binding in place, removing nothing.
    let skip: HashSet<PathBuf> = [super::canonical_or_original(&c.path)]
        .into_iter()
        .collect();
    let prune = mgr
        .cleanup_project("p-2", &skip, &mut Vec::new())
        .expect("cleanup p-2 skipping c");
    assert_eq!(prune.removed, 0, "the live worktree was skipped");
    assert_eq!(prune.skipped.len(), 1);
    assert!(c.path.exists(), "a skipped worktree is left on disk");
    assert_eq!(
        mgr.store().load_worktrees().expect("load").len(),
        1,
        "a skipped worktree keeps its binding"
    );

    // With nothing skipped, p-2 is finally removed.
    assert_eq!(
        mgr.cleanup_project("p-2", &no_skip, &mut Vec::new())
            .expect("cleanup p-2")
            .removed,
        1
    );
    assert!(!c.path.exists());

    // Pruning a project with no owned worktrees is a no-op.
    assert_eq!(
        mgr.cleanup_project("p-unknown", &no_skip, &mut Vec::new())
            .expect("cleanup unknown")
            .removed,
        0
    );
}

// The worktree-binding store round-trip, two-branch coexistence, and
// owner-private file mode are now covered by the unified store's own unit tests
// in `crate::store` (the binding records and store moved there in M9).

// --- credential redaction -------------------------------------------------

#[test]
fn redact_url_credentials_strips_userinfo_from_git_error_output() {
    use super::redact_url_credentials;

    // The dominant real-world leak: a PAT as the URL username (git does NOT
    // redact this) echoed in a "could not read Password" failure.
    let token = "https://ghp_SUPERSECRETTOKEN@github.com";
    let msg = format!("fatal: could not read Password for '{token}': terminal prompts disabled");
    let redacted = redact_url_credentials(&msg);
    assert!(
        !redacted.contains("ghp_SUPERSECRETTOKEN"),
        "token must be redacted: {redacted}"
    );
    assert!(
        redacted.contains("https://<redacted>@github.com"),
        "host must survive redaction: {redacted}"
    );

    // user:password form is also scrubbed.
    let userpass =
        redact_url_credentials("clone of https://alice:hunter2@example.com/x.git failed");
    assert!(
        !userpass.contains("hunter2"),
        "password redacted: {userpass}"
    );
    assert!(!userpass.contains("alice"), "username redacted: {userpass}");
    assert!(userpass.contains("https://<redacted>@example.com/x.git"));

    // A credential-free URL is left untouched.
    let clean = "fatal: repository 'https://github.com/org/repo' not found";
    assert_eq!(redact_url_credentials(clean), clean);

    // A message with no URL is left untouched.
    let plain = "fatal: not a git repository";
    assert_eq!(redact_url_credentials(plain), plain);
}

#[test]
fn redact_url_credentials_covers_ssh_and_multiple_urls() {
    use super::redact_url_credentials;

    // `ssh://user@host` userinfo is redacted (the user can be a sensitive login).
    let ssh = redact_url_credentials("ssh://deploybot@git.example.com:22/org/repo.git");
    assert!(!ssh.contains("deploybot"), "ssh userinfo redacted: {ssh}");
    assert!(ssh.contains("ssh://<redacted>@git.example.com:22/org/repo.git"));

    // Every URL-shaped substring in one message is scrubbed independently.
    let many = redact_url_credentials(
        "tried https://tok1@a.example.com/x then https://u:tok2@b.example.com/y",
    );
    assert!(!many.contains("tok1") && !many.contains("tok2"), "{many}");
    assert!(many.contains("https://<redacted>@a.example.com/x"));
    assert!(many.contains("https://<redacted>@b.example.com/y"));
}

#[test]
fn redact_url_credentials_security_boundary_scp_and_query_are_out_of_scope() {
    use super::redact_url_credentials;

    // SCP-form `git@host:path` is left verbatim ON PURPOSE: it has no `://`, and
    // the `git@` is a username for key-based SSH auth, never a secret. Pinning it
    // documents the boundary so a future change does not "helpfully" mangle it.
    let scp = "git@github.com:org/repo.git";
    assert_eq!(redact_url_credentials(scp), scp, "SCP form is not a secret");

    // Query/fragment tokens are NOT redacted: the authority ends at `?`/`#` and
    // native git never authenticates through them. This asserts the *intentional*
    // gap so it is a conscious, reviewed boundary — not an accident. Should a real
    // query-string-credential integration ever appear, this test must change first.
    let query = redact_url_credentials("https://example.com/x?token=SECRET");
    assert!(
        query.contains("token=SECRET"),
        "query token out of scope: {query}"
    );
    let fragment = redact_url_credentials("https://example.com/x#SECRET");
    assert!(
        fragment.contains("#SECRET"),
        "fragment out of scope: {fragment}"
    );
}

// --- lifecycle hooks (Part B) --------------------------------------------

/// Commit a hook script at `<repo>/.pohunek/hooks/<event>` on the current branch,
/// so a worktree created from it has the hook checked out too.
fn commit_hook(repo: &Path, event: &str, body: &str) {
    let dir = repo.join(".pohunek/hooks");
    fs::create_dir_all(&dir).expect("create hooks dir");
    fs::write(dir.join(event), body).expect("write hook");
    git_in(repo, &["add", "."]);
    git_in(repo, &["commit", "-q", "-m", &format!("add {event} hook")]);
}

/// Parse a `KEY=VALUE`-per-line dump (the output of `env`) into a map.
fn parse_env_dump(text: &str) -> std::collections::HashMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

#[test]
fn post_create_hook_runs_with_cleared_env_and_exact_allowlist() {
    // The load-bearing security test: a hook must run with a CLEARED environment
    // plus only the B.3.1 allowlist — never the daemon's inherited env (which holds
    // GITHUB_TOKEN/ANTHROPIC_API_KEY/POHUNEK_SOCKET_PATH). The test binary always
    // runs under cargo, so `CARGO_*` is present in the daemon process env; its
    // absence in the hook env proves `.env_clear()` fired.
    let mgr = manager("hook-env");
    let repo = init_repo("hook-env-repo");
    commit_hook(
        &repo,
        "post-create",
        "#!/bin/sh\nenv > \"$POHUNEK_REPO/hook-env.txt\"\n",
    );
    let bound = mgr
        .bind(&request("s-hook", &repo, "feat/x"))
        .expect("bind runs post-create hook");
    assert!(bound.warnings.is_empty(), "a passing hook warns nothing");

    let dump = fs::read_to_string(repo.join("hook-env.txt")).expect("hook wrote its env");
    let env = parse_env_dump(&dump);

    // The B.3.1 allowlist is present with the right values.
    assert_eq!(
        env.get("POHUNEK_HOOK_EVENT").map(String::as_str),
        Some("post-create")
    );
    assert_eq!(
        env.get("POHUNEK_SESSION_ID").map(String::as_str),
        Some("s-hook")
    );
    assert_eq!(env.get("POHUNEK_AGENT").map(String::as_str), Some("claude"));
    assert_eq!(
        env.get("POHUNEK_BRANCH").map(String::as_str),
        Some("feat/x")
    );
    assert!(env.contains_key("POHUNEK_REPO"));
    assert!(env.contains_key("POHUNEK_WORKTREE"));
    assert!(env.contains_key("POHUNEK_BASE_BRANCH"));
    assert!(env.contains_key("PATH"), "PATH is passed through");

    // env_clear proof: NO inherited daemon env survives.
    assert!(
        !env.keys().any(|k| k.starts_with("CARGO")),
        "the daemon's inherited env (CARGO_*) must be cleared: {:?}",
        env.keys().collect::<Vec<_>>()
    );
    // The daemon handshake vars must NEVER reach a hook.
    for forbidden in [
        "POHUNEK_SOCKET_PATH",
        "POHUNEK_DAEMON_ID",
        "POHUNEK_ENV",
        "POHUNEK_PROTOCOL_VERSION",
    ] {
        assert!(
            !env.contains_key(forbidden),
            "handshake var {forbidden} must not be exposed to a hook"
        );
    }
}

#[test]
fn post_create_hook_in_hooks_dir_shadows_legacy_setup() {
    // The B.1 matrix: with BOTH `.pohunek/hooks/post-create` and `.pohunek/setup`
    // committed, only post-create runs (never both).
    let mgr = manager("hook-shadow");
    let repo = init_repo("hook-shadow-repo");
    fs::create_dir_all(repo.join(".pohunek/hooks")).expect("hooks dir");
    fs::write(
        repo.join(".pohunek/hooks/post-create"),
        "#!/bin/sh\ntouch \"$POHUNEK_WORKTREE/post-create-ran\"\n",
    )
    .expect("write hook");
    fs::write(
        repo.join(".pohunek/setup"),
        "#!/bin/sh\ntouch \"$PWD/setup-ran\"\n",
    )
    .expect("write setup");
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-q", "-m", "hook + setup"]);

    let bound = mgr.bind(&request("s-1", &repo, "feat/x")).expect("bind");
    assert!(
        bound.path.join("post-create-ran").exists(),
        "the hooks/post-create script runs"
    );
    assert!(
        !bound.path.join("setup-ran").exists(),
        "the legacy setup is shadowed by post-create (never both)"
    );
}

#[test]
fn reused_worktree_fires_no_create_hook() {
    // A post-create hook appends a line each run; reusing an owned worktree (the
    // early-return) must fire NO create hook, so the count stays 1.
    let mgr = manager("hook-reuse");
    let repo = init_repo("hook-reuse-repo");
    commit_hook(
        &repo,
        "post-create",
        "#!/bin/sh\necho ran >> \"$POHUNEK_REPO/create-count\"\n",
    );
    mgr.bind(&request("s-1", &repo, "feat/x"))
        .expect("first bind");
    let again = mgr
        .bind(&request("s-1", &repo, "feat/x"))
        .expect("reuse bind");
    assert!(again.reused, "second bind reuses the worktree");

    let count = fs::read_to_string(repo.join("create-count")).expect("counter exists");
    assert_eq!(
        count.lines().count(),
        1,
        "the create hook fires only on the fresh-create path"
    );
}

#[test]
fn remove_hooks_fire_on_cleanup_with_post_remove_in_repository() {
    // pre-remove fires IN the worktree (before removal); post-remove fires IN the
    // repository (after). Both see POHUNEK_AGENT from the persisted binding. Markers
    // are written into POHUNEK_REPO (which persists past the worktree removal).
    let mgr = manager("hook-remove");
    let repo = init_repo("hook-remove-repo");
    commit_hook(
        &repo,
        "pre-remove",
        "#!/bin/sh\necho \"$POHUNEK_AGENT\" > \"$POHUNEK_REPO/pre-remove-agent\"\n",
    );
    commit_hook(
        &repo,
        "post-remove",
        "#!/bin/sh\ntouch \"$POHUNEK_REPO/post-remove-ran\"\n",
    );
    mgr.bind(&request("s-1", &repo, "feat/x")).expect("bind");

    let mut warnings = Vec::new();
    let removed = mgr.cleanup_session("s-1", &mut warnings).expect("cleanup");
    assert_eq!(removed, 1);
    assert!(warnings.is_empty(), "passing remove hooks warn nothing");
    assert_eq!(
        fs::read_to_string(repo.join("pre-remove-agent"))
            .expect("pre-remove ran")
            .trim(),
        "claude",
        "pre-remove sees POHUNEK_AGENT from the binding"
    );
    assert!(
        repo.join("post-remove-ran").exists(),
        "post-remove runs in the repository after removal"
    );
}

#[test]
fn host_global_and_in_repo_post_create_run_host_global_first() {
    // B3 compose: when both layers have a post-create hook, both run, host-global
    // FIRST, then in-repo (append-ordered marker proves ordering).
    let config_dir = unique_dir("hook-host-config");
    fs::create_dir_all(config_dir.join("hooks")).expect("host hooks dir");
    fs::write(
        config_dir.join("hooks/post-create"),
        "#!/bin/sh\necho host >> \"$POHUNEK_REPO/order\"\n",
    )
    .expect("write host hook");
    let mgr = manager_with_config_dir("hook-host", config_dir);
    let repo = init_repo("hook-host-repo");
    commit_hook(
        &repo,
        "post-create",
        "#!/bin/sh\necho repo >> \"$POHUNEK_REPO/order\"\n",
    );

    mgr.bind(&request("s-1", &repo, "feat/x")).expect("bind");
    let order = fs::read_to_string(repo.join("order")).expect("order file");
    assert_eq!(
        order.lines().collect::<Vec<_>>(),
        vec!["host", "repo"],
        "host-global hook runs before the in-repo hook"
    );
}
