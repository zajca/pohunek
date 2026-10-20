//! Unit tests for the worktree binder, store, slug, ownership, and the three
//! non-fatal warning paths. Each test that needs a repository builds a real,
//! throwaway git repo under the system temp dir (git is required, as it is for
//! the daemon at runtime).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::{SessionRetentionHold, SessionWarningKind};

use super::{
    admin_lock, git_command, lock_within, run_admin_command_within, worktree_add_new,
    worktree_prune,
};
use super::{
    branch_slug, is_valid_worktree, run_output_bounded, HookStderrCapture, WorktreeCleanup,
    WorktreeManager, WorktreeRequest, WorktreeWork,
};
use crate::store::{Store, WorktreeStatus};

/// Generous setup-script timeout for tests whose script finishes promptly; long
/// enough that it never trips on a slow CI box.
const TEST_SETUP_TIMEOUT: Duration = Duration::from_secs(30);

/// A fresh directory that is removed when the test's thread ends.
fn unique_dir(tag: &str) -> PathBuf {
    crate::test_support::thread_scoped_dir(&format!("pohunek-wt-{tag}-"))
}

fn git_in(dir: &Path, args: &[&str]) {
    let output = pohunek_test_support::process_env::command("git")
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
    let init = pohunek_test_support::process_env::command("git")
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
    let clone = pohunek_test_support::process_env::command("git")
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

fn manager_with_timeout(tag: &str, setup_timeout: Duration) -> WorktreeManager {
    let root = unique_dir(&format!("{tag}-root"));
    let store = unique_dir(&format!("{tag}-store")).join("metadata.jsonl");
    WorktreeManager::new(root, Arc::new(Store::new(store)), setup_timeout, None, None)
}

/// A manager with a host-global hook layer rooted at `config_dir`.
fn manager_with_config_dir(tag: &str, config_dir: PathBuf) -> WorktreeManager {
    let root = unique_dir(&format!("{tag}-root"));
    let store = unique_dir(&format!("{tag}-store")).join("metadata.jsonl");
    WorktreeManager::new(
        root,
        Arc::new(Store::new(store)),
        TEST_SETUP_TIMEOUT,
        None,
        Some(config_dir),
    )
}

/// Run git in `dir` and return trimmed stdout (asserting success).
fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let output = pohunek_test_support::process_env::command("git")
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

#[test]
fn bounded_command_times_out_and_kills_a_slow_child() {
    let mut cmd = pohunek_test_support::process_env::command("sh");
    cmd.args(["-c", "sleep 5"]);

    let started = Instant::now();
    let err =
        run_output_bounded(cmd, Duration::from_millis(100)).expect_err("slow command times out");

    assert!(
        started.elapsed() < Duration::from_secs(2),
        "timeout runner should return promptly"
    );
    assert!(
        err.contains("timed out"),
        "timeout error should explain the deadline: {err}"
    );
}

#[cfg(unix)]
#[test]
fn bounded_command_timeout_kills_process_group_children() {
    let dir = unique_dir("bounded-command-process-group");
    let marker = dir.join("child-survived");
    let script = format!("(sleep 0.4; printf leaked > '{}') & wait", marker.display());
    let mut cmd = pohunek_test_support::process_env::command("sh");
    cmd.args(["-c", &script]);

    let err =
        run_output_bounded(cmd, Duration::from_millis(100)).expect_err("process group times out");

    assert!(
        err.contains("timed out"),
        "timeout error should explain the deadline: {err}"
    );
    std::thread::sleep(Duration::from_millis(700));
    assert!(
        !marker.exists(),
        "timeout kill must terminate background children too"
    );
}

#[test]
fn bounded_command_drains_output_larger_than_pipe_buffer() {
    let mut cmd = pohunek_test_support::process_env::command("sh");
    cmd.args(["-c", "head -c 200000 /dev/zero | tr '\\0' x"]);

    let output = run_output_bounded(cmd, TEST_SETUP_TIMEOUT).expect("large output drains");

    assert!(output.status.success());
    assert_eq!(output.stdout.len(), 200_000);
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
    let clone = pohunek_test_support::process_env::command("git")
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
fn remote_only_base_branch_is_fetched_before_default_branch_fallback() {
    let upstream = init_repo("remote-base-upstream");
    git_in(&upstream, &["checkout", "-q", "-b", "stacked/base"]);
    fs::write(upstream.join("STACKED.md"), "stacked\n").expect("write stacked file");
    git_in(&upstream, &["add", "."]);
    git_in(&upstream, &["commit", "-q", "-m", "stacked base"]);
    let stacked_tip = git_stdout(&upstream, &["rev-parse", "HEAD"]);
    git_in(&upstream, &["checkout", "-q", "main"]);

    let downstream = unique_dir("remote-base-downstream-parent").join("clone");
    let clone = pohunek_test_support::process_env::command("git")
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
    assert_ne!(
        git_stdout(&downstream, &["branch", "--list", "stacked/base"]),
        "stacked/base",
        "the requested base must not exist as a local branch before binding"
    );

    let mgr = manager("remote-base");
    let mut req = request("s-1", &downstream, "feat/x");
    req.base_branch = Some("stacked/base".to_owned());
    let bound = mgr.bind(&req).expect("bind fetches the remote-only base");

    assert_eq!(bound.base_branch, "stacked/base");
    assert!(
        bound.warnings.is_empty(),
        "a successful remote-only base fetch must not warn: {:?}",
        bound.warnings
    );
    assert_eq!(git_stdout(&bound.path, &["rev-parse", "HEAD"]), stacked_tip);
}

/// Number of concurrent creates in the shared-repository race test.
const CONCURRENT_CREATES: usize = 8;

/// Clone `upstream` into a fresh directory configured for committing.
fn clone_of(upstream: &Path, tag: &str) -> PathBuf {
    let downstream = unique_dir(tag).join("clone");
    let clone = pohunek_test_support::process_env::command("git")
        .args(["clone", "-q"])
        .arg(upstream)
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
    downstream
}

/// Refs under the per-request fetch namespace, one per line.
fn leftover_fetch_refs(repo: &Path) -> String {
    git_stdout(repo, &["for-each-ref", "refs/pohunek/fetch/"])
}

#[test]
fn concurrent_creates_in_one_repository_each_start_from_their_own_base() {
    // Upstream carries distinct commits behind distinct refs: remote-only
    // branches and `refs/pull/<n>/head`-style refs. The downstream clone has
    // none of them locally, so every create fetches its own base.
    let upstream = init_repo("race-upstream");
    let mut bases = Vec::new();
    for index in 0..CONCURRENT_CREATES {
        git_in(
            &upstream,
            &["checkout", "-q", "-b", &format!("tmp/{index}"), "main"],
        );
        fs::write(
            upstream.join(format!("BASE-{index}.md")),
            format!("{index}\n"),
        )
        .expect("write base file");
        git_in(&upstream, &["add", "."]);
        git_in(&upstream, &["commit", "-q", "-m", &format!("base {index}")]);
        let tip = git_stdout(&upstream, &["rev-parse", "HEAD"]);
        let name = if index % 2 == 0 {
            format!("stacked/base-{index}")
        } else {
            format!("refs/pull/{index}/head")
        };
        let refname = if name.starts_with("refs/") {
            name.clone()
        } else {
            format!("refs/heads/{name}")
        };
        git_in(&upstream, &["update-ref", &refname, &tip]);
        git_in(&upstream, &["checkout", "-q", "main"]);
        git_in(&upstream, &["branch", "-q", "-D", &format!("tmp/{index}")]);
        bases.push((name, tip));
    }
    let downstream = clone_of(&upstream, "race-downstream");

    let mgr = manager("race");
    let barrier = std::sync::Barrier::new(CONCURRENT_CREATES);
    let results: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = bases
            .iter()
            .enumerate()
            .map(|(index, (base, _))| {
                let (mgr, barrier, downstream) = (&mgr, &barrier, &downstream);
                scope.spawn(move || {
                    let mut req =
                        request(&format!("s-{index}"), downstream, &format!("feat/{index}"));
                    req.base_branch = Some(base.clone());
                    barrier.wait();
                    mgr.bind(&req)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("create thread"))
            .collect()
    });

    for ((base, tip), result) in bases.iter().zip(results) {
        let bound = result.expect("concurrent bind succeeds");
        assert_eq!(
            &bound.base_branch, base,
            "the logical base name is kept; warnings: {:#?}",
            bound.warnings
        );
        assert!(
            bound.warnings.is_empty(),
            "no warnings expected: {:?}",
            bound.warnings
        );
        assert_eq!(
            &git_stdout(&bound.path, &["rev-parse", "HEAD"]),
            tip,
            "worktree for base {base} must start from that base's commit"
        );
    }
    assert_eq!(leftover_fetch_refs(&downstream), "");
}

/// Barrier-aligned rounds of concurrent adds in the raw-git race test.
const RAW_ADD_ROUNDS: usize = 6;

/// Concurrent `git worktree add` calls per round in the raw-git race test.
const RAW_ADDS_PER_ROUND: usize = 12;

#[test]
fn concurrent_worktree_adds_and_prunes_in_one_repository_all_succeed() {
    let repo = init_repo("raw-race");
    let checkouts = unique_dir("raw-race-checkouts");

    for round in 0..RAW_ADD_ROUNDS {
        // One extra participant prunes while the adds run: prune reads and may
        // delete admin entries, so it must not observe a half-written one.
        let barrier = std::sync::Barrier::new(RAW_ADDS_PER_ROUND + 1);
        let (adds, prune) = std::thread::scope(|scope| {
            let prune = scope.spawn(|| {
                barrier.wait();
                worktree_prune(&repo)
            });
            let handles: Vec<_> = (0..RAW_ADDS_PER_ROUND)
                .map(|index| {
                    let (repo, barrier) = (&repo, &barrier);
                    let path = checkouts.join(format!("wt-{round}-{index}"));
                    scope.spawn(move || {
                        barrier.wait();
                        worktree_add_new(repo, &path, &format!("b/{round}-{index}"), "main")
                            .map(|()| path)
                    })
                })
                .collect();
            let adds: Vec<_> = handles
                .into_iter()
                .map(|handle| handle.join().expect("add thread"))
                .collect();
            (adds, prune.join().expect("prune thread"))
        });

        prune.expect("concurrent prune succeeds");
        for added in adds {
            let path = added.expect("concurrent worktree add succeeds");
            assert!(
                is_valid_worktree(&path),
                "{} must stay valid",
                path.display()
            );
        }
    }
    let listed = git_stdout(&repo, &["worktree", "list", "--porcelain"]);
    assert_eq!(
        listed.matches("\nworktree ").count(),
        RAW_ADD_ROUNDS * RAW_ADDS_PER_ROUND,
        "every added worktree is still registered:\n{listed}"
    );
}

#[test]
fn admin_lock_is_shared_per_repository_and_independent_across_repositories() {
    let first = init_repo("lock-first");
    let second = init_repo("lock-second");
    let linked = unique_dir("lock-linked").join("wt");
    worktree_add_new(&first, &linked, "feat/linked", "main").expect("add a linked worktree");

    let first_lock = admin_lock(&first).expect("lock for the first repository");
    let via_linked = admin_lock(&linked).expect("lock via a linked worktree");
    let second_lock = admin_lock(&second).expect("lock for the second repository");
    assert!(
        Arc::ptr_eq(&first_lock, &via_linked),
        "the main checkout and its linked worktree share one lock"
    );
    assert!(
        !Arc::ptr_eq(&first_lock, &second_lock),
        "distinct repositories use distinct locks"
    );

    let held = lock_within(&first_lock, Duration::ZERO).expect("take the first lock");
    assert!(
        lock_within(&via_linked, Duration::ZERO).is_err(),
        "a second operation on the same repository waits for the holder"
    );
    // Another repository is unaffected while the first is held.
    let other = std::thread::scope(|scope| {
        scope
            .spawn(|| lock_within(&second_lock, Duration::ZERO).map(drop))
            .join()
            .expect("lock thread")
    });
    other.expect("a different repository is not serialized behind the first");
    drop(held);
    drop(lock_within(&first_lock, Duration::ZERO).expect("the lock is free after release"));
}

#[test]
fn admin_lock_wait_reports_a_timeout_instead_of_blocking_forever() {
    let repo = init_repo("lock-timeout");
    let lock = admin_lock(&repo).expect("lock");
    let _held = lock_within(&lock, Duration::ZERO).expect("take the lock");

    let message = lock_within(&lock, Duration::ZERO).expect_err("lock is held");

    assert!(
        message.contains("timed out"),
        "unexpected message: {message}"
    );
}

#[test]
fn admin_commands_wait_for_the_repository_lock_before_running_git() {
    let repo = init_repo("lock-wiring");
    let checkout = unique_dir("lock-wiring-checkout").join("wt");
    let lock = admin_lock(&repo).expect("lock");
    let held = lock_within(&lock, Duration::ZERO).expect("take the lock");

    let mut cmd = git_command(&repo).expect("git command");
    cmd.args(["worktree", "add", "-b", "feat/locked", "--end-of-options"])
        .arg(&checkout)
        .arg("main");
    let message = run_admin_command_within(&repo, cmd, Duration::ZERO)
        .expect_err("the held lock keeps the command from running");

    assert!(
        message.contains("timed out"),
        "unexpected message: {message}"
    );
    assert!(
        !checkout.exists(),
        "git must not have run while the lock was held"
    );
    drop(held);
}

/// Writes the signal file on drop so a failing test still ends the hook's
/// background job.
#[cfg(unix)]
struct ReleaseOnDrop(PathBuf);

#[cfg(unix)]
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        let _ = fs::write(&self.0, "go\n");
    }
}

/// Bound on the second add's wait for the repository lock; only reached when
/// the lock is wrongly held for as long as the first call is joining output.
#[cfg(unix)]
const TEST_LOCK_WAIT: Duration = Duration::from_secs(10);

#[cfg(unix)]
#[test]
fn repository_lock_is_released_when_git_exits_even_if_a_hook_descendant_holds_the_output() {
    use std::os::unix::fs::PermissionsExt;

    let repo = init_repo("lock-exit");
    let state = unique_dir("lock-exit-state");
    let started = state.join("started");
    let release = state.join("release");
    let hook = repo.join(".git").join("hooks").join("post-checkout");
    fs::create_dir_all(hook.parent().expect("hooks dir")).expect("hooks dir");
    // Only the first checkout leaves a background job; it inherits git's output
    // pipes and exits when the test writes the release file.
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\nif [ ! -e '{started}' ]; then\n  : > '{started}'\n  \
             (while [ ! -e '{release}' ]; do sleep 0.05; done) &\nfi\nexit 0\n",
            started = started.display(),
            release = release.display(),
        ),
    )
    .expect("write hook");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod hook");
    let first_path = unique_dir("lock-exit-first").join("wt");
    let second_path = unique_dir("lock-exit-second").join("wt");
    let add = |path: &Path, branch: &str, lock_wait: Duration| {
        let mut cmd = git_command(&repo).expect("git command");
        cmd.args(["worktree", "add", "-b", branch, "--end-of-options"])
            .arg(path)
            .arg("main");
        run_admin_command_within(&repo, cmd, lock_wait)
    };

    std::thread::scope(|scope| {
        // Dropped on unwind too, before the scope waits for the first call.
        let _release = ReleaseOnDrop(release.clone());
        let first = scope.spawn(|| add(&first_path, "feat/first", TEST_LOCK_WAIT));
        pohunek_test_support::wait::poll_until("the first hook started its background job", || {
            started.exists().then_some(())
        });

        // Git has exited, so the lock must be free before the bounded drain ends.
        add(&second_path, "feat/second", TEST_LOCK_WAIT)
            .expect("second add proceeds while the first call is still joining output");
        pohunek_test_support::wait::poll_until(
            "bounded completion before descendant release",
            || first.is_finished().then_some(()),
        );

        fs::write(&release, "go\n").expect("release the descendant");
        first
            .join()
            .expect("first add thread")
            .expect("first add succeeds");
    });
    assert!(is_valid_worktree(&first_path));
    assert!(is_valid_worktree(&second_path));
}

#[test]
fn a_bind_finishes_while_a_same_group_git_grandchild_holds_output() {
    bind_with_pipe_holding_grandchild(false, false);
}

#[test]
fn a_bind_finishes_while_a_setsid_git_grandchild_holds_output() {
    bind_with_pipe_holding_grandchild(true, false);
}

#[test]
fn a_failing_bind_reports_partial_stderr_while_a_git_grandchild_holds_output() {
    bind_with_pipe_holding_grandchild(false, true);
}

fn bind_with_pipe_holding_grandchild(detached: bool, fails: bool) {
    use std::os::unix::fs::PermissionsExt;

    let repo = init_repo("bounded-native-hook");
    let state = unique_dir("bounded-native-hook-state");
    let started = state.join("started");
    let release = state.join("release");
    let ended = state.join("ended");
    let python = crate::agent::trusted_program("python3").expect("test Python");
    let hook = repo.join(".git/hooks/post-checkout");
    fs::write(&hook, format!(
        "#!{}\nimport os,time,pathlib\ns=pathlib.Path({:?})\nif (s/'started').exists(): os._exit(0)\nif os.fork()==0:\n{}    (s/'started').write_text('ready')\n    os.write(1,b'partial stdout\\n')\n    os.write(2,b'partial stderr\\n')\n    while not (s/'release').exists(): time.sleep(0.01)\n    try: os.write(1,b'late output')\n    except BrokenPipeError: (s/'ended').write_text('closed')\n    else: (s/'ended').write_text('open')\n    os._exit(0)\nos._exit({})\n",
        python.display(), state.to_str().expect("test state path"),
        if detached { "    os.setsid()\n" } else { "" },
        if fails { 7 } else { 0 },
    )).expect("write native hook");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("executable hook");
    let manager = manager("bounded-native-hook");
    let req = request("first", &repo, "feat/first");
    let logs = crate::runtime::lifecycle::tests::LogCapture::default();
    std::thread::scope(|scope| {
        // Release on failure before joining, so the unfixed runner fails rather than hangs.
        let _release = ReleaseOnDrop(release.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        let manager = &manager;
        let req = &req;
        let logs = &logs;
        let first = scope.spawn(move || {
            let _subscriber = logs.install();
            let _ = tx.send(manager.bind(req));
        });
        pohunek_test_support::wait::poll_until("native hook grandchild ready", || {
            started.exists().then_some(())
        });
        let lock = admin_lock(&repo).expect("repository lock");
        drop(lock_within(&lock, TEST_LOCK_WAIT).expect("git exit releases lock before drain"));
        manager
            .bind(&request("second", &repo, "feat/second"))
            .expect("second bind proceeds");
        let result = rx.recv_timeout(Duration::from_secs(3));
        assert!(
            result.is_ok(),
            "bind must finish before the pipe holder is released: {result:?}"
        );
        let result = result.expect("bounded result");
        if fails {
            let error = result.expect_err("native hook exit 7 fails the bind");
            assert_eq!(error.code, "worktree_add_failed");
            assert!(error.msg.contains("partial stderr"), "{error:?}");
        } else {
            result.expect("status-only worktree add succeeds");
        }
        assert!(!ended.exists(), "no post-reap signal ended the grandchild");
        fs::write(&release, "go").expect("release grandchild");
        first.join().expect("bind thread");
        pohunek_test_support::wait::poll_until("grandchild exits naturally", || {
            ended.exists().then_some(())
        });
        assert_eq!(fs::read_to_string(&ended).expect("pipe result"), "closed");
    });
    assert_eq!(
        logs.text()
            .matches("git output incomplete; proceeding")
            .count(),
        1
    );
}

#[test]
fn an_oversized_failing_native_hook_keeps_bounded_partial_diagnostics() {
    use std::os::unix::fs::PermissionsExt;
    let repo = init_repo("native-hook-cap");
    let python = crate::agent::trusted_program("python3").expect("test Python");
    let hook = repo.join(".git/hooks/post-checkout");
    fs::write(&hook, format!("#!{}\nimport sys\nsys.stderr.write('diagnostic marker\\n'+'x'*(9*1024*1024))\nsys.exit(7)\n", python.display())).expect("write hook");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("executable hook");
    let logs = crate::runtime::lifecycle::tests::LogCapture::default();
    let _subscriber = logs.install();
    let error = manager("native-hook-cap")
        .bind(&request("first", &repo, "feat/first"))
        .expect_err("native hook fails");
    assert!(error.msg.contains("diagnostic marker"));
    assert!(
        error.msg.len() < 8 * 1024 * 1024 + 1024,
        "retained diagnostics are capped"
    );
    assert_eq!(
        logs.text()
            .matches("git output incomplete; proceeding")
            .count(),
        1
    );
}

#[test]
fn admin_lock_survives_a_panicking_holder() {
    let repo = init_repo("lock-poison");
    let lock = admin_lock(&repo).expect("lock");
    let poisoner = Arc::clone(&lock);
    let panicked = std::thread::spawn(move || {
        let _guard = poisoner.lock().expect("take the lock");
        panic!("holder panics while holding the admin lock");
    })
    .join();
    assert!(panicked.is_err(), "the holder thread panicked");

    drop(lock_within(&lock, Duration::ZERO).expect("a poisoned lock is still usable"));
}

#[test]
fn successful_fetch_leaves_no_temporary_ref_and_keeps_the_logical_base_name() {
    let upstream = init_repo("pin-upstream");
    let downstream = clone_of(&upstream, "pin-downstream");
    fs::write(upstream.join("NEW.md"), "v2\n").expect("write upstream file");
    git_in(&upstream, &["add", "."]);
    git_in(&upstream, &["commit", "-q", "-m", "v2"]);

    let mgr = manager("pin");
    let mut req = request("s-1", &downstream, "feat/x");
    req.base_branch = Some("main".to_owned());
    let bound = mgr.bind(&req).expect("bind");

    assert_eq!(bound.base_branch, "main");
    let stored = mgr
        .store()
        .find_worktree_for_session("s-1")
        .expect("read binding")
        .expect("binding persisted");
    assert_eq!(stored.base_branch, "main");
    assert_eq!(leftover_fetch_refs(&downstream), "");
}

#[test]
fn failed_fetch_leaves_no_temporary_ref() {
    let repo = init_repo("fetch-clean-repo");
    let bogus = unique_dir("fetch-clean-bogus").join("missing.git");
    git_in(
        &repo,
        &["remote", "add", "origin", &bogus.to_string_lossy()],
    );

    let mgr = manager("fetch-clean");
    let bound = mgr
        .bind(&request("s-1", &repo, "feat/x"))
        .expect("bind falls back when fetch fails");

    assert!(bound
        .warnings
        .iter()
        .any(|w| w.kind == SessionWarningKind::Fetch));
    assert_eq!(leftover_fetch_refs(&repo), "");
}

#[test]
fn base_ref_with_refspec_syntax_is_not_fetched() {
    let upstream = init_repo("refspec-upstream");
    let downstream = clone_of(&upstream, "refspec-downstream");
    let before = git_stdout(&downstream, &["for-each-ref"]);

    for hostile in ["main:refs/heads/injected", "+main", "ma*in"] {
        assert!(
            super::fetch_origin(&downstream, hostile).is_err(),
            "{hostile:?} must be rejected before any fetch"
        );
    }
    assert_eq!(git_stdout(&downstream, &["for-each-ref"]), before);
}

#[test]
fn fetched_commit_stays_referenced_until_the_worktree_is_created() {
    // The pre-create hook runs after the fetch and before `git worktree add`;
    // it records the fetch refs that still exist at that point.
    let upstream = init_repo("pinned-upstream");
    git_in(&upstream, &["checkout", "-q", "-b", "stacked/base"]);
    fs::write(upstream.join("STACKED.md"), "stacked\n").expect("write stacked file");
    git_in(&upstream, &["add", "."]);
    git_in(&upstream, &["commit", "-q", "-m", "stacked base"]);
    let tip = git_stdout(&upstream, &["rev-parse", "HEAD"]);
    git_in(&upstream, &["checkout", "-q", "main"]);
    let downstream = clone_of(&upstream, "pinned-downstream");
    commit_hook(
        &downstream,
        "pre-create",
        "#!/bin/sh\ngit for-each-ref --format='%(objectname)' refs/pohunek/fetch/ > \"$POHUNEK_REPO/pinned.txt\"\n",
    );

    let mgr = manager("pinned");
    let mut req = request("s-1", &downstream, "feat/x");
    req.base_branch = Some("stacked/base".to_owned());
    let bound = mgr.bind(&req).expect("bind");

    assert_eq!(
        fs::read_to_string(downstream.join("pinned.txt"))
            .expect("hook ran")
            .trim(),
        tip,
        "the fetch ref must pin the commit while the pre-create hook runs"
    );
    assert_eq!(git_stdout(&bound.path, &["rev-parse", "HEAD"]), tip);
    assert_eq!(leftover_fetch_refs(&downstream), "");
}

#[test]
fn annotated_tag_base_is_pinned_by_its_commit_and_its_ref_is_deleted() {
    let upstream = init_repo("tag-upstream");
    git_in(&upstream, &["tag", "-a", "v1.0.0", "-m", "release"]);
    let commit = git_stdout(&upstream, &["rev-parse", "v1.0.0^{commit}"]);
    let tag_object = git_stdout(&upstream, &["rev-parse", "v1.0.0"]);
    assert_ne!(commit, tag_object, "an annotated tag has its own object");
    let downstream = clone_of(&upstream, "tag-downstream");
    git_in(&downstream, &["tag", "-d", "v1.0.0"]);

    let fetched = super::fetch_origin(&downstream, "v1.0.0").expect("fetch the tag");
    assert_eq!(fetched.commit, commit);
    drop(fetched);
    assert_eq!(leftover_fetch_refs(&downstream), "");

    let mgr = manager("tag");
    let mut req = request("s-1", &downstream, "feat/x");
    req.base_branch = Some("v1.0.0".to_owned());
    let bound = mgr.bind(&req).expect("bind from a tag base");
    assert_eq!(git_stdout(&bound.path, &["rev-parse", "HEAD"]), commit);
    assert_eq!(leftover_fetch_refs(&downstream), "");
}

#[test]
fn fetch_never_overwrites_or_deletes_an_existing_destination_ref() {
    let upstream = init_repo("clobber-upstream");
    let downstream = clone_of(&upstream, "clobber-downstream");
    let main_tip = git_stdout(&downstream, &["rev-parse", "HEAD"]);
    fs::write(downstream.join("OTHER.md"), "other\n").expect("write file");
    git_in(&downstream, &["add", "."]);
    git_in(&downstream, &["commit", "-q", "-m", "other"]);
    let other_tip = git_stdout(&downstream, &["rev-parse", "HEAD"]);

    // One destination holds an unrelated commit, the other the very commit the
    // fetch would produce; neither may be touched.
    for (dest, oid) in [
        ("refs/pohunek/fetch/taken-other", &other_tip),
        ("refs/pohunek/fetch/taken-same", &main_tip),
    ] {
        git_in(&downstream, &["update-ref", dest, oid]);
        super::fetch_into_ref(&downstream, "main", dest)
            .expect_err("an existing destination must fail the fetch");
        assert_eq!(&git_stdout(&downstream, &["rev-parse", dest]), oid);
    }
}

#[test]
fn fetch_disables_automatic_maintenance() {
    let repo = init_repo("fetch-maintenance");
    let cmd = super::fetch_command(&repo, "main", "refs/pohunek/fetch/x").expect("command");
    let args: Vec<_> = cmd.get_args().map(|arg| arg.to_string_lossy()).collect();
    let fetch = args
        .iter()
        .position(|arg| arg == "fetch")
        .expect("fetch verb");
    for setting in ["gc.auto=0", "maintenance.auto=false"] {
        let at = args
            .iter()
            .position(|arg| arg == setting)
            .unwrap_or_else(|| panic!("{setting} missing: {args:?}"));
        assert!(at < fetch, "{setting} must precede the verb: {args:?}");
    }
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
fn a_requested_base_that_does_not_exist_fails_instead_of_using_the_default_branch() {
    let mgr = manager("base-missing");
    let repo = init_repo("base-missing-repo");
    let mut req = request("s-1", &repo, "feat/x");
    req.base_branch = Some("release/does-not-exist".to_owned());

    let err = mgr.bind(&req).expect_err("a missing base fails the create");

    assert_eq!(err.code, "invalid_base_branch", "{err:?}");
    assert!(err.msg.contains("release/does-not-exist"), "{err:?}");
    assert!(
        !mgr.path_for(&req.session_id, &repo, "feat-x").exists(),
        "no worktree is created on another base"
    );
}

/// A repository whose `origin` cannot be fetched from.
fn repo_with_an_unreachable_origin(tag: &str) -> PathBuf {
    let repo = init_repo(tag);
    let bogus = unique_dir(&format!("{tag}-bogus")).join("missing.git");
    git_in(
        &repo,
        &["remote", "add", "origin", &bogus.to_string_lossy()],
    );
    repo
}

#[test]
fn a_requested_base_that_cannot_be_fetched_fails_naming_the_fetch_failure() {
    let mgr = manager("base-fetch-fails");
    let repo = repo_with_an_unreachable_origin("base-fetch-fails-repo");
    let mut req = request("s-1", &repo, "feat/x");
    req.base_branch = Some("stacked/base-4".to_owned());

    let err = mgr
        .bind(&req)
        .expect_err("a base that is neither local nor fetchable fails the create");

    assert_eq!(err.code, "base_branch_fetch_failed", "{err:?}");
    assert!(err.msg.contains("stacked/base-4"), "{err:?}");
    assert!(
        err.msg.contains("fetching it from origin failed"),
        "{err:?}"
    );
    assert!(
        err.msg.contains("does not appear to be a git repository"),
        "git's own failure is named: {err:?}"
    );
    assert!(
        !mgr.path_for(&req.session_id, &repo, "feat-x").exists(),
        "no worktree is created on another base"
    );
}

#[test]
fn a_local_requested_base_is_used_when_its_fetch_fails_with_a_fetch_warning_only() {
    let mgr = manager("base-local-fetch-fails");
    let repo = repo_with_an_unreachable_origin("base-local-fetch-fails-repo");
    git_in(&repo, &["branch", "release"]);
    git_in(&repo, &["checkout", "-q", "release"]);
    git_in(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "release moves on"],
    );
    let release = git_stdout(&repo, &["rev-parse", "refs/heads/release"]);
    git_in(&repo, &["checkout", "-q", "main"]);
    let mut req = request("s-1", &repo, "feat/x");
    req.base_branch = Some("release".to_owned());

    let bound = mgr
        .bind(&req)
        .expect("a local base survives a failed fetch");

    assert_eq!(bound.base_branch, "release");
    assert_eq!(git_stdout(&bound.path, &["rev-parse", "HEAD"]), release);
    assert!(
        bound
            .warnings
            .iter()
            .any(|w| w.kind == SessionWarningKind::Fetch),
        "the failed fetch is reported: {:?}",
        bound.warnings
    );
    assert!(
        bound
            .warnings
            .iter()
            .all(|w| w.kind != SessionWarningKind::BaseBranchFallback),
        "the requested base was not replaced: {:?}",
        bound.warnings
    );
}

#[test]
fn a_create_without_a_base_still_starts_from_the_current_branch_when_fetch_fails() {
    let mgr = manager("no-base-fetch-fails");
    let repo = repo_with_an_unreachable_origin("no-base-fetch-fails-repo");

    let bound = mgr
        .bind(&request("s-1", &repo, "feat/x"))
        .expect("a request without a base falls back to the current branch");

    assert_eq!(bound.base_branch, "main");
}

/// A repository whose `feat/x` branch is one commit behind `main`, and the
/// two commits.
fn repo_with_a_branch_behind_main(tag: &str) -> (PathBuf, String, String) {
    let repo = init_repo(tag);
    git_in(&repo, &["branch", "feat/x"]);
    git_in(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "main moves on"],
    );
    let branch = git_stdout(&repo, &["rev-parse", "refs/heads/feat/x"]);
    let main = git_stdout(&repo, &["rev-parse", "refs/heads/main"]);
    assert_ne!(branch, main);
    (repo, branch, main)
}

#[test]
fn an_explicit_base_ignored_for_an_existing_branch_at_another_commit_is_reported() {
    let mgr = manager("base-ignored");
    let (repo, branch_commit, main_commit) = repo_with_a_branch_behind_main("base-ignored-repo");
    let mut req = request("s-1", &repo, "feat/x");
    req.base_branch = Some("main".to_owned());

    let bound = mgr.bind(&req).expect("bind checks the existing branch out");

    assert_eq!(
        git_stdout(&bound.path, &["rev-parse", "HEAD"]),
        branch_commit,
        "the existing branch is checked out as it is"
    );
    let warning = bound
        .warnings
        .iter()
        .find(|w| w.kind == SessionWarningKind::BaseBranchFallback)
        .expect("base-branch warning present");
    assert!(warning.message.contains("feat/x"), "{}", warning.message);
    assert!(warning.message.contains("main"), "{}", warning.message);
    let detail = warning.detail.as_deref().expect("warning detail");
    assert!(detail.contains(&branch_commit), "{detail}");
    assert!(detail.contains(&main_commit), "{detail}");
}

#[test]
fn a_local_branch_behind_its_origin_named_as_its_own_base_is_reported() {
    // Adopting a pull request's head branch: `--branch H --base-branch H`,
    // where the local `H` is behind `origin/H`.
    let upstream = init_repo("adopt-upstream");
    git_in(&upstream, &["checkout", "-q", "-b", "pr/head"]);
    git_in(
        &upstream,
        &["commit", "-q", "--allow-empty", "-m", "first head commit"],
    );
    git_in(&upstream, &["checkout", "-q", "main"]);
    let downstream = unique_dir("adopt-downstream-parent").join("clone");
    let clone = pohunek_test_support::process_env::command("git")
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
    git_in(&downstream, &["branch", "pr/head", "origin/pr/head"]);
    let local_commit = git_stdout(&downstream, &["rev-parse", "refs/heads/pr/head"]);
    git_in(&upstream, &["checkout", "-q", "pr/head"]);
    git_in(
        &upstream,
        &["commit", "-q", "--allow-empty", "-m", "second head commit"],
    );
    let origin_commit = git_stdout(&upstream, &["rev-parse", "HEAD"]);
    git_in(&upstream, &["checkout", "-q", "main"]);

    let mut req = request("s-1", &downstream, "pr/head");
    req.base_branch = Some("pr/head".to_owned());
    let bound = manager("adopt")
        .bind(&req)
        .expect("bind the local head branch");

    assert_eq!(
        git_stdout(&bound.path, &["rev-parse", "HEAD"]),
        local_commit,
        "the local branch is checked out as it is"
    );
    let warning = bound
        .warnings
        .iter()
        .find(|w| w.kind == SessionWarningKind::BaseBranchFallback)
        .expect("base-branch warning present");
    let detail = warning.detail.as_deref().expect("warning detail");
    assert!(detail.contains(&local_commit), "{detail}");
    assert!(detail.contains(&origin_commit), "{detail}");
}

#[test]
fn a_branch_a_pre_create_hook_moves_off_the_base_is_reported_as_checked_out() {
    let repo = init_repo("base-moved-repo");
    commit_hook(
        &repo,
        "pre-create",
        "#!/bin/sh\ngit update-ref refs/heads/feat/y \"$(git rev-parse main~1)\"\n",
    );
    git_in(&repo, &["branch", "feat/y", "main"]);
    let moved_to = git_stdout(&repo, &["rev-parse", "main~1"]);
    let mut req = request("s-1", &repo, "feat/y");
    req.base_branch = Some("main".to_owned());

    let bound = manager("base-moved")
        .bind(&req)
        .expect("bind the moved branch");

    assert_eq!(git_stdout(&bound.path, &["rev-parse", "HEAD"]), moved_to);
    let warning = bound
        .warnings
        .iter()
        .find(|w| w.kind == SessionWarningKind::BaseBranchFallback)
        .expect("base-branch warning present");
    let detail = warning.detail.as_deref().expect("warning detail");
    assert!(detail.contains(&moved_to), "{detail}");
}

#[test]
fn a_tag_named_like_the_local_base_does_not_decide_the_base_commit() {
    let (repo, branch_commit, _main_commit) = repo_with_a_branch_behind_main("base-tag-repo");
    // The existing branch sits on the base branch's commit, and a tag with the
    // base's name points elsewhere.
    git_in(&repo, &["branch", "-f", "feat/x", "main"]);
    git_in(&repo, &["tag", "main", &branch_commit]);
    let mut req = request("s-1", &repo, "feat/x");
    req.base_branch = Some("main".to_owned());

    let bound = manager("base-tag")
        .bind(&req)
        .expect("bind at the base commit");

    assert!(
        bound
            .warnings
            .iter()
            .all(|w| w.kind != SessionWarningKind::BaseBranchFallback),
        "{:?}",
        bound.warnings
    );
}

#[test]
fn an_existing_branch_without_an_explicit_base_or_at_the_base_commit_is_not_reported() {
    let (repo, _branch_commit, _main_commit) = repo_with_a_branch_behind_main("base-kept-repo");

    // No explicit base: an existing branch is the ordinary resume case.
    let bound = manager("base-default")
        .bind(&request("s-1", &repo, "feat/x"))
        .expect("bind without an explicit base");
    assert!(
        bound
            .warnings
            .iter()
            .all(|w| w.kind != SessionWarningKind::BaseBranchFallback),
        "{:?}",
        bound.warnings
    );

    // An explicit base at the branch's own commit changes nothing.
    git_in(&repo, &["branch", "feat/y", "main"]);
    let mut req = request("s-2", &repo, "feat/y");
    req.base_branch = Some("main".to_owned());
    let bound = manager("base-same")
        .bind(&req)
        .expect("bind at the base commit");
    assert!(
        bound
            .warnings
            .iter()
            .all(|w| w.kind != SessionWarningKind::BaseBranchFallback),
        "{:?}",
        bound.warnings
    );
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
        let alive = pohunek_test_support::process_env::command("kill")
            .arg("-0")
            .arg(pid.to_string())
            .output()
            .is_ok_and(|o| o.status.success());
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
        let _ = fs::set_permissions(&store_dir, fs::Permissions::from_mode(0o700));
        eprintln!(
            "skipping binding_persist_failure_rolls_back_the_worktree: perms not enforced (root?)"
        );
        return;
    }

    let result = mgr.bind(&request("s-1", &repo, "feat/x"));

    // Restore perms first so the temp dir is cleanable regardless of the outcome.
    fs::set_permissions(&store_dir, fs::Permissions::from_mode(0o700))
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
    assert_eq!(none, WorktreeCleanup::default());
    assert!(
        bound.path.exists(),
        "an unowned session must not touch our tree"
    );

    // Owned session: the tree and its binding are removed.
    let removed = mgr
        .cleanup_session("s-1", &mut Vec::new())
        .expect("cleanup owned");
    assert_eq!(
        removed,
        WorktreeCleanup {
            removed: 1,
            failed: 0
        }
    );
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
    assert_eq!(
        removed,
        WorktreeCleanup {
            removed: 1,
            failed: 0
        }
    );
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
fn post_remove_skips_with_warning_when_repository_is_gone() {
    let mgr = manager("hook-remove-repo-gone");
    let repo = init_repo("hook-remove-repo-gone-repo");
    mgr.bind(&request("s-1", &repo, "feat/x")).expect("bind");
    fs::remove_dir_all(&repo).expect("delete repository before cleanup");

    let mut warnings = Vec::new();
    let removed = mgr.cleanup_session("s-1", &mut warnings).expect("cleanup");

    // The repository is gone, so `git worktree remove` cannot run: the binding is
    // dropped anyway and the failure is reported rather than counted as cleaned.
    assert_eq!(
        removed,
        WorktreeCleanup {
            removed: 0,
            failed: 1
        }
    );
    let warning = warnings
        .iter()
        .find(|warning| warning.kind == SessionWarningKind::Hook)
        .expect("post-remove skip warning");
    assert!(
        warning.message.contains("post-remove"),
        "warning summary should identify the hook event: {warning:?}"
    );
    let detail = warning.detail.as_deref().unwrap_or_default();
    assert!(
        detail.contains("no longer exists") && detail.contains("post-remove hook skipped"),
        "warning detail should explain that post-remove was skipped: {warning:?}"
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

#[test]
fn each_create_hook_logs_its_start_and_its_duration_and_outcome() {
    let logs = crate::runtime::lifecycle::tests::LogCapture::default();
    let _subscriber = logs.install();
    let config_dir = unique_dir("hook-timing-config");
    fs::create_dir_all(config_dir.join("hooks")).expect("host hooks dir");
    fs::write(config_dir.join("hooks/post-create"), "#!/bin/sh\nexit 0\n")
        .expect("write host hook");
    let mgr = manager_with_config_dir("hook-timing", config_dir);
    let repo = init_repo("hook-timing-repo");
    commit_hook(&repo, "post-create", "#!/bin/sh\nexit 3\n");

    mgr.bind(&request("s-timed", &repo, "feat/x"))
        .expect("bind");

    let text = logs.text();
    for (layer, outcome) in [("host", "succeeded"), ("repository", "failed")] {
        let lines = |message: &str| {
            text.lines()
                .filter(|line| {
                    line.contains(message)
                        && line.contains("session_id=s-timed")
                        && line.contains("event=\"post-create\"")
                        && line.contains(&format!("layer=\"{layer}\""))
                })
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        let started = lines("hook started");
        assert_eq!(started.len(), 1, "{layer} hook start: {text}");
        assert!(started[0].contains("timeout_ms="), "{}", started[0]);
        let finished = lines("hook finished");
        assert_eq!(finished.len(), 1, "{layer} hook end: {text}");
        assert!(finished[0].contains("duration_ms="), "{}", finished[0]);
        assert!(
            finished[0].contains(&format!("outcome=\"{outcome}\"")),
            "{}",
            finished[0]
        );
    }
}

/// Release grace of the held-stderr scenario: short, so the hand-off follows the
/// hook within moments.
const HELD_STDERR_RELEASE_GRACE: Duration = Duration::from_millis(200);

/// The `drain_pid` field of the hand-off line.
fn drain_pid(line: &str) -> rustix::process::Pid {
    line.split_whitespace()
        .find_map(|field| field.strip_prefix("drain_pid="))
        .and_then(|pid| pid.parse::<i32>().ok())
        .and_then(rustix::process::Pid::from_raw)
        .unwrap_or_else(|| panic!("the hand-off names the drain: {line}"))
}

/// The pipe a process holds as descriptor `fd`, as `/proc` names it.
#[cfg(target_os = "linux")]
fn pipe_of(pid: rustix::process::Pid, fd: u32) -> String {
    fs::read_link(format!("/proc/{}/fd/{fd}", pid.as_raw_nonzero()))
        .expect("read the descriptor link")
        .display()
        .to_string()
}

/// Asserts that `drain` reads the pipe `holder` writes as stderr, has
/// `/dev/null` as stdout and stderr, inherited no descriptor this process
/// holds, and that this process no longer holds the pipe's read end.
#[cfg(target_os = "linux")]
fn assert_drain_descriptors(holder: rustix::process::Pid, drain: rustix::process::Pid) {
    // The drain holds the writer's pipe as stdin and `/dev/null` as stdout
    // and stderr. Any higher descriptor is the drain's own (GNU cat opens a
    // splice pipe), never one this process holds.
    let pipe = pipe_of(holder, 2);
    assert_eq!(pipe_of(drain, 0), pipe, "the drain reads the writer's pipe");
    for fd in [1, 2] {
        assert_eq!(pipe_of(drain, fd), "/dev/null");
    }
    let own: Vec<String> = fs::read_dir("/proc/self/fd")
        .expect("list own descriptors")
        .filter_map(Result::ok)
        .filter_map(|entry| fs::read_link(entry.path()).ok())
        .map(|target| target.display().to_string())
        .collect();
    for entry in fs::read_dir(format!("/proc/{}/fd", drain.as_raw_nonzero()))
        .expect("list the drain's descriptors")
        .filter_map(Result::ok)
    {
        let fd: u32 = entry
            .file_name()
            .to_string_lossy()
            .parse()
            .expect("numeric descriptor");
        if fd > 2 {
            let target = pipe_of(drain, fd);
            assert!(
                !own.contains(&target),
                "the drain inherited descriptor {fd} ({target}) from the daemon"
            );
        }
    }
    pohunek_test_support::wait::poll_until("the reader closes its read end", || {
        let held = fs::read_dir("/proc/self/fd")
            .expect("list own descriptors")
            .filter_map(Result::ok)
            .any(|entry| {
                fs::read_link(entry.path()).is_ok_and(|target| target.display().to_string() == pipe)
            });
        (!held).then_some(())
    });
}

/// The numbered descriptors `pid` holds, as `lsof` lists them.
#[cfg(target_os = "macos")]
fn lsof_descriptors(pid: rustix::process::Pid) -> Vec<u32> {
    let output = std::process::Command::new("/usr/sbin/lsof")
        .args([
            "-a",
            "-p",
            &pid.as_raw_nonzero().to_string(),
            "-d",
            "0-65535",
            "-F",
            "f",
        ])
        .output()
        .expect("run lsof");
    let mut descriptors: Vec<u32> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix('f'))
        .filter_map(|fd| fd.parse().ok())
        .collect();
    descriptors.sort_unstable();
    descriptors
}

#[test]
fn a_background_process_holding_a_create_hook_stderr_gets_a_drain_process() {
    // A hook that leaves a process running with its stderr inherited: after the
    // release grace the daemon hands the pipe's read end to a drain process and
    // closes its own, so the process keeps a readable stderr when the daemon
    // exits. The log names the event and script, never the hook's output.
    let logs = crate::runtime::lifecycle::tests::LogCapture::default();
    let _subscriber = logs.install();
    let config_dir = unique_dir("hook-held-config");
    fs::create_dir_all(config_dir.join("hooks")).expect("host hooks dir");
    let holder_pid = config_dir.join("holder.pid");
    let holder_writes = config_dir.join("holder-writes");
    fs::write(
        config_dir.join("hooks/post-create"),
        format!(
            "#!/bin/sh
echo 'hook output stays private' >&2
             (echo $$ > /dev/null; while :; do echo tick >&2 || exit 1; echo w >> '{writes}'; sleep 0.1; done) &
             echo $! > '{holder}'
",
            writes = holder_writes.display(),
            holder = holder_pid.display()
        ),
    )
    .expect("write host hook");
    let mgr = WorktreeManager::new(
        unique_dir("hook-held-root"),
        Arc::new(Store::new(
            unique_dir("hook-held-store").join("metadata.jsonl"),
        )),
        TEST_SETUP_TIMEOUT,
        Some(HookStderrCapture {
            tail: std::num::NonZeroUsize::new(4096).expect("non-zero tail"),
            release_grace: HELD_STDERR_RELEASE_GRACE,
        }),
        Some(config_dir.clone()),
    );
    let repo = init_repo("hook-held-repo");
    // A descriptor without close-on-exec, as a library could leave one: the
    // drain must not inherit it.
    let inheritable = fs::File::open(&config_dir).expect("open a descriptor");
    rustix::io::fcntl_setfd(&inheritable, rustix::io::FdFlags::empty())
        .expect("clear close-on-exec");

    let bound = mgr.bind(&request("s-1", &repo, "feat/x")).expect("bind");
    assert!(
        bound.warnings.is_empty() && bound.stderr_tails.is_empty(),
        "a successful hook leaves no warning and no tail: {:?}",
        bound.warnings
    );
    let line = pohunek_test_support::wait::poll_until("the drain hand-off", || {
        logs.text()
            .lines()
            .find(|line| line.contains("a drain process reads it from now on"))
            .map(str::to_owned)
    });
    assert!(line.contains("DEBUG"), "{line}");
    assert!(line.contains("post-create"), "{line}");
    assert!(
        line.contains(&config_dir.join("hooks/post-create").display().to_string()),
        "{line}"
    );
    assert!(!logs.text().contains("hook output stays private"));
    let drain = drain_pid(&line);
    let holder = fs::read_to_string(&holder_pid)
        .ok()
        .and_then(|pid| pid.trim().parse::<i32>().ok())
        .and_then(rustix::process::Pid::from_raw)
        .expect("the hook recorded its background writer");

    #[cfg(target_os = "linux")]
    assert_drain_descriptors(holder, drain);
    #[cfg(target_os = "macos")]
    assert_eq!(
        lsof_descriptors(drain),
        [0, 1, 2],
        "the drain inherits only its stdio"
    );

    let writes = || fs::read_to_string(&holder_writes).map_or(0, |text| text.lines().count());
    let before = writes();
    pohunek_test_support::wait::poll_until("the writer keeps writing to stderr", || {
        (writes() > before + 3).then_some(())
    });

    let _ = rustix::process::kill_process(holder, rustix::process::Signal::KILL);
    pohunek_test_support::wait::poll_until("the drain ends and is reaped", || {
        rustix::process::test_kill_process(drain)
            .is_err()
            .then_some(())
    });
}

// --- retention safety probe -----------------------------------------------

/// Asserts `unsaved_work` holds the session for exactly `expected`.
fn assert_held(mgr: &WorktreeManager, session: &str, expected: SessionRetentionHold) {
    match mgr.unsaved_work(session) {
        WorktreeWork::Held { hold, detail } => {
            assert_eq!(hold, expected, "detail: {detail}");
            assert!(!detail.is_empty(), "a hold always explains itself");
        }
        WorktreeWork::Saved => panic!("expected a {expected:?} hold, got Saved"),
    }
}

#[test]
fn unsaved_work_clears_a_clean_owned_worktree() {
    let mgr = manager("unsaved-clean");
    let repo = init_repo("unsaved-clean-repo");
    mgr.bind(&request("s-1", &repo, "feat/x")).expect("bind");

    assert_eq!(
        mgr.unsaved_work("s-1"),
        WorktreeWork::Saved,
        "a freshly bound worktree branched off main holds nothing"
    );
}

#[test]
fn unsaved_work_clears_a_session_with_no_binding() {
    let mgr = manager("unsaved-unbound");

    assert_eq!(
        mgr.unsaved_work("s-unknown"),
        WorktreeWork::Saved,
        "a session with no owned worktree has nothing to lose"
    );
}

#[test]
fn unsaved_work_holds_a_worktree_with_an_uncommitted_change() {
    let mgr = manager("unsaved-dirty");
    let repo = init_repo("unsaved-dirty-repo");
    let bound = mgr.bind(&request("s-1", &repo, "feat/x")).expect("bind");
    fs::write(bound.path.join("README.md"), "edited by the agent\n").expect("dirty a tracked file");

    assert_held(&mgr, "s-1", SessionRetentionHold::WorktreeUncommitted);
}

#[test]
fn unsaved_work_holds_a_worktree_with_an_untracked_file() {
    let mgr = manager("unsaved-untracked");
    let repo = init_repo("unsaved-untracked-repo");
    let bound = mgr.bind(&request("s-1", &repo, "feat/x")).expect("bind");
    fs::write(bound.path.join("notes.md"), "scratch work\n").expect("add an untracked file");

    assert_held(&mgr, "s-1", SessionRetentionHold::WorktreeUntracked);
}

#[test]
fn unsaved_work_holds_a_worktree_whose_commit_is_in_no_other_ref() {
    let mgr = manager("unsaved-unpushed");
    let repo = init_repo("unsaved-unpushed-repo");
    let bound = mgr.bind(&request("s-1", &repo, "feat/x")).expect("bind");
    fs::write(bound.path.join("README.md"), "committed only here\n").expect("edit tracked file");
    git_in(&bound.path, &["add", "."]);
    git_in(&bound.path, &["commit", "-q", "-m", "work in progress"]);

    assert_held(&mgr, "s-1", SessionRetentionHold::WorktreeUnpushed);
}

#[test]
fn unsaved_work_clears_a_commit_another_ref_still_contains() {
    let mgr = manager("unsaved-pushed");
    let repo = init_repo("unsaved-pushed-repo");
    let bound = mgr.bind(&request("s-1", &repo, "feat/x")).expect("bind");
    fs::write(bound.path.join("README.md"), "committed and shared\n").expect("edit tracked file");
    git_in(&bound.path, &["add", "."]);
    git_in(&bound.path, &["commit", "-q", "-m", "shared work"]);
    // A second ref on the same commit stands in for a pushed branch: the commit
    // survives the checkout's removal either way.
    git_in(&bound.path, &["branch", "review/feat-x"]);

    assert_eq!(mgr.unsaved_work("s-1"), WorktreeWork::Saved);
}

#[test]
fn unsaved_work_holds_a_worktree_git_cannot_read() {
    let mgr = manager("unsaved-unreadable");
    let repo = init_repo("unsaved-unreadable-repo");
    let bound = mgr.bind(&request("s-1", &repo, "feat/x")).expect("bind");
    // The `.git` file is what makes the directory a worktree; without it git
    // cannot report anything about the checkout that is still on disk.
    fs::remove_file(bound.path.join(".git")).expect("drop the worktree gitdir link");

    assert_held(&mgr, "s-1", SessionRetentionHold::WorktreeUnknown);
}

#[test]
fn unsaved_work_clears_a_binding_whose_checkout_is_already_gone() {
    let mgr = manager("unsaved-vanished");
    let repo = init_repo("unsaved-vanished-repo");
    let bound = mgr.bind(&request("s-1", &repo, "feat/x")).expect("bind");
    fs::remove_dir_all(&bound.path).expect("delete the checkout behind git's back");

    assert_eq!(
        mgr.unsaved_work("s-1"),
        WorktreeWork::Saved,
        "nothing is on disk, so removal destroys nothing"
    );
}

#[test]
fn status_hold_ranks_an_uncommitted_change_above_an_untracked_file() {
    let hold = super::status_hold("?? notes.md\n M README.md\n").expect("a hold");

    assert_eq!(hold.0, SessionRetentionHold::WorktreeUncommitted);
    assert!(
        hold.1.contains("README.md"),
        "detail names the file: {}",
        hold.1
    );
}

#[test]
fn native_hook_deadline_diagnostics_redact_a_split_credential_url() {
    partial_credential_diagnostics_are_redacted(true);
}

#[test]
fn native_hook_cap_diagnostics_redact_a_split_credential_url() {
    partial_credential_diagnostics_are_redacted(false);
}

fn partial_credential_diagnostics_are_redacted(deadline: bool) {
    use std::os::unix::fs::PermissionsExt;
    let repo = init_repo("partial-url-redaction");
    let state = unique_dir("partial-url-state");
    let release = state.join("release");
    let ended = state.join("ended");
    let _release = ReleaseOnDrop(release.clone());
    let python = crate::agent::trusted_program("python3").expect("test Python");
    let hook = repo.join(".git/hooks/post-checkout");
    let suffix = if deadline {
        format!("if os.fork()==0:\n    while not pathlib.Path({:?}).exists(): time.sleep(0.01)\n    try: os.write(2,b'@example.invalid/repo.git')\n    except BrokenPipeError: pass\n    pathlib.Path({:?}).write_text('done')\n    os._exit(0)\n", release.to_str().expect("release path"), ended.to_str().expect("ended path"))
    } else {
        "os.write(2,b'x'*(9*1024*1024)+b'@example.invalid/repo.git')\n".to_owned()
    };
    fs::write(&hook, format!("#!{}\nimport os,time,pathlib\nos.write(2,b'partial diagnostic https://FAKE_REMOTE_USERINFO')\n{suffix}os._exit(7)\n", python.display())).expect("write native hook");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("executable hook");
    let logs = crate::runtime::lifecycle::tests::LogCapture::default();
    let _subscriber = logs.install();
    let error = manager("partial-url")
        .bind(&request("first", &repo, "feat/first"))
        .expect_err("hook exit 7 fails bind");
    fs::write(&release, "go").expect("release writer");
    if deadline {
        pohunek_test_support::wait::poll_until("writer exits naturally", || {
            ended.exists().then_some(())
        });
    }
    assert_eq!(error.code, "worktree_add_failed");
    assert!(error.msg.contains("partial diagnostic"));
    assert!(
        !error.msg.contains("FAKE_REMOTE_USERINFO"),
        "partial credentials must never escape the capture boundary"
    );
    assert!(error.msg.contains("<redacted>"));
    assert!(!logs.text().contains("FAKE_REMOTE_USERINFO"));
}

#[test]
fn a_fetch_with_a_pipe_holding_hook_uses_the_remote_tip_and_removes_its_ref() {
    use std::os::unix::fs::PermissionsExt;
    let upstream = init_repo("held-fetch-upstream");
    let downstream = clone_of(&upstream, "held-fetch-downstream");
    git_in(
        &upstream,
        &["commit", "--allow-empty", "-q", "-m", "new upstream tip"],
    );
    let expected = git_stdout(&upstream, &["rev-parse", "HEAD"]);
    let state = unique_dir("held-fetch-state");
    let release = state.join("release");
    let started = state.join("started");
    let ended = state.join("ended");
    let _release = ReleaseOnDrop(release.clone());
    let python = crate::agent::trusted_program("python3").expect("test Python");
    let hook = downstream.join(".git/hooks/reference-transaction");
    fs::write(&hook, format!("#!{}\nimport os,sys,time,pathlib\ns=pathlib.Path({:?})\nrefs=sys.stdin.read()\nif sys.argv[1]=='committed' and ' refs/pohunek/fetch/' in refs and not (s/'started').exists():\n    (s/'started').write_text('ready')\n    if os.fork()==0:\n        while not (s/'release').exists(): time.sleep(0.01)\n        (s/'ended').write_text('done')\n        os._exit(0)\nos._exit(0)\n", python.display(), state.to_str().expect("state path"))).expect("write fetch hook");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("executable hook");
    let result = manager("held-fetch").bind(&request("first", &downstream, "feat/first"));
    fs::write(&release, "go").expect("release fetch writer");
    assert!(started.exists(), "the fetch transaction hook ran");
    pohunek_test_support::wait::poll_until("fetch descendant exits naturally", || {
        ended.exists().then_some(())
    });
    let bound = result.expect("bind succeeds after successful fetch");
    assert_eq!(
        git_stdout(&bound.path, &["rev-parse", "HEAD"]),
        expected,
        "incomplete diagnostics must not select the stale local base"
    );
    assert!(
        git_stdout(
            &downstream,
            &["for-each-ref", "--format=%(refname)", "refs/pohunek/fetch"]
        )
        .is_empty(),
        "the owned temporary ref is removed"
    );
    assert!(bound
        .warnings
        .iter()
        .all(|warning| warning.kind != SessionWarningKind::Fetch));
}
