//! Pure git-repo detection (design `projects.md` → "Detection algorithm").
//!
//! Given a working directory, [`detect`] feels out git on that host and returns
//! a [`DetectedProject`] — the repository's identity (`git_common_dir`), its main
//! checkout, this work tree's root, whether this is a linked worktree, the
//! current branch, and the `origin` URL — or `None` when the directory is not a
//! git work tree. It shells out to `git` exactly as the design specifies; **every
//! step is non-fatal**: any failure (git absent, slow, a non-git directory)
//! yields `None` so a session still starts, just unregistered.
//!
//! [`project_id`] derives a project's stable id from its canonical
//! `git_common_dir` via FNV-1a — dependency-free and deterministic across
//! restarts, so the id need never be persisted (the path is the key, the id is a
//! collision-resistant short handle for the CLI).
//!
//! Requires **git >= 2.31** for `rev-parse --path-format=absolute`. On an older
//! git that flag errors, [`detect`] returns `Ok(None)`, and the directory is
//! simply left unregistered — consistent with the non-fatal contract, but no
//! project ever auto-registers on such a host until git is upgraded. To keep that
//! diagnosable rather than silent, the first time the flag fails inside a
//! confirmed work tree a one-time `warn!` fires (see `warn_git_too_old`); behavior
//! is unchanged.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use crate::worktree::{canonical_or_original, redact_url_credentials};

/// Wall-clock bound on a single detection `git` call. Detection runs on the
/// session-start hot path, so a wedged/slow `git` must never block it: a call
/// that outlives this is terminated and treated as a failure (→ no project),
/// exactly like any other non-fatal detection failure. Generous enough that a
/// healthy `git` returning a path or branch name never trips it.
const DETECT_GIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Poll interval while waiting on a detection `git` child to exit. Small enough
/// that a fast `git` returns promptly, large enough that the busy loop is
/// negligible against [`DETECT_GIT_TIMEOUT`]. Mirrors the worktree setup-script
/// poll discipline.
const DETECT_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// FNV-1a 64-bit offset basis.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A repository detected from a working directory.
///
/// Keyed (for the store) on [`Self::git_common_dir`]: the main checkout and every
/// linked worktree of one repository share it, so they collapse to one logical
/// project. All paths are canonical and absolute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedProject {
    /// The git common dir — the project's identity key (canonical, absolute).
    pub git_common_dir: PathBuf,
    /// The repository's main checkout (`checkout_path` for a non-linked work
    /// tree; the main worktree otherwise; the common dir for a bare repo).
    pub repo_root: PathBuf,
    /// This work tree's root (the directory `cwd` lives in, canonicalized).
    pub checkout_path: PathBuf,
    /// Whether `cwd` is inside a linked worktree rather than the main checkout.
    pub is_linked_worktree: bool,
    /// Whether the repository is bare (no working tree).
    pub is_bare: bool,
    /// The currently checked-out branch; `None` on a detached HEAD / mid-rebase.
    pub branch: Option<String>,
    /// The `origin` remote URL, credentials redacted; `None` when unset.
    pub origin_url: Option<String>,
}

/// Derive a project's stable id from its canonical `git_common_dir`.
///
/// `"p-"` + the first 8 hex digits of the FNV-1a 64-bit hash of the key's bytes.
/// FNV-1a is dependency-free and deterministic across restarts and processes, so
/// the id is reproducible without persisting a counter: the path is the key, the
/// id is a short, collision-resistant handle surfaced only when two projects on
/// one host share a label (design Decision 2). The caller passes the *canonical*
/// common dir so two spellings of one repo (symlinks) yield one id.
#[must_use]
pub fn project_id(git_common_dir: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;

    let mut hash = FNV_OFFSET_BASIS;
    for &byte in git_common_dir.as_os_str().as_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    let hex = format!("{hash:016x}");
    format!("p-{}", &hex[..8])
}

/// Detect the project (if any) rooted at `cwd`.
///
/// Returns `Ok(Some(project))` for a git work tree (or a bare repo), `Ok(None)`
/// for a non-git directory **or any non-fatal detection failure** (git missing,
/// a call timing out, a malformed response). The `io::Result` leaves room for a
/// genuine I/O fault to surface loudly, but the design mandates that detection
/// never aborts a session, so today every failure path returns `Ok(None)`.
///
/// Runs blocking `git` subprocesses; call it from `spawn_blocking`.
pub fn detect(cwd: &Path) -> io::Result<Option<DetectedProject>> {
    // Step 1: is this a work tree? A bare repo answers "false" here but "true"
    // to `--is-bare-repository`; anything else (a non-git dir, git absent) is
    // simply "no project".
    let inside_work_tree = git(cwd, &["rev-parse", "--is-inside-work-tree"]);
    if inside_work_tree.as_deref() != Some("true") {
        if git(cwd, &["rev-parse", "--is-bare-repository"]).as_deref() == Some("true") {
            return Ok(detect_bare(cwd));
        }
        return Ok(None);
    }

    // Step 2: the project key — the (absolute) git common dir, canonicalized so
    // symlinked checkouts converge to one project.
    let Some(common_dir) = git(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ) else {
        // Step 1 already confirmed a work tree (git is present and this is a repo),
        // yet `--path-format=absolute` failed: almost certainly git < 2.31, on which
        // no project ever auto-registers. Warn once so it is diagnosable; stay
        // non-fatal (return `Ok(None)`, the session still starts).
        warn_git_too_old();
        return Ok(None);
    };
    let git_common_dir = canonical_or_original(Path::new(&common_dir));

    // Step 3: a linked worktree's git dir differs from the common dir.
    let Some(git_dir) = git(cwd, &["rev-parse", "--path-format=absolute", "--git-dir"]) else {
        return Ok(None);
    };
    let is_linked_worktree = canonical_or_original(Path::new(&git_dir)) != git_common_dir;

    // Step 4: this work tree's root.
    let Some(toplevel) = git(cwd, &["rev-parse", "--show-toplevel"]) else {
        return Ok(None);
    };
    let checkout_path = canonical_or_original(Path::new(&toplevel));

    // Step 5: the main checkout. For a linked worktree the authoritative source
    // is the first `git worktree list --porcelain` entry (it always names the
    // main worktree, even for relocated/separate-git-dir layouts); fall back to
    // the common dir's parent (`.../repo/.git` → `.../repo`).
    let repo_root = if is_linked_worktree {
        main_worktree(cwd)
            .or_else(|| git_common_dir.parent().map(Path::to_path_buf))
            .map(|root| canonical_or_original(&root))
            .unwrap_or_else(|| checkout_path.clone())
    } else {
        checkout_path.clone()
    };

    Ok(Some(DetectedProject {
        git_common_dir,
        repo_root,
        checkout_path,
        is_linked_worktree,
        is_bare: false,
        // Step 6: current branch; a detached HEAD / mid-rebase has none.
        branch: git(cwd, &["symbolic-ref", "--short", "HEAD"]),
        // Step 7: origin URL, credentials redacted before it can be stored,
        // sent on the wire, or surfaced to an agent.
        origin_url: origin_url(cwd),
    }))
}

/// Detection for a bare repository (no working tree). The common dir is the
/// repository itself, so `repo_root` and `checkout_path` both point at it; the
/// `is_bare` flag tells the UI not to promise an in-place checkout.
fn detect_bare(cwd: &Path) -> Option<DetectedProject> {
    let Some(common_dir) = git(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ) else {
        // Same too-old-git signal as in `detect`: `--is-bare-repository` answered
        // "true" (so git ran), but the absolute-path query failed. Warn once.
        warn_git_too_old();
        return None;
    };
    let git_common_dir = canonical_or_original(Path::new(&common_dir));
    Some(DetectedProject {
        repo_root: git_common_dir.clone(),
        checkout_path: git_common_dir.clone(),
        is_linked_worktree: false,
        is_bare: true,
        branch: git(cwd, &["symbolic-ref", "--short", "HEAD"]),
        origin_url: origin_url(cwd),
        git_common_dir,
    })
}

/// The `origin` remote URL with any embedded credentials redacted.
fn origin_url(cwd: &Path) -> Option<String> {
    git(cwd, &["remote", "get-url", "origin"]).map(|url| redact_url_credentials(&url))
}

/// The main worktree's path from `git worktree list --porcelain` (its first
/// `worktree <path>` line), or `None` if the command fails or is empty.
fn main_worktree(cwd: &Path) -> Option<PathBuf> {
    // `-z` makes the output NUL-delimited and, crucially, emits paths VERBATIM:
    // non-`-z` porcelain C-quotes any path containing a newline/tab/quote (wraps
    // it in `"…"` with backslash escapes), which would corrupt repo_root for an
    // exotically-named checkout. A filesystem path can never contain a NUL, so
    // splitting on NUL is unambiguous; the first `worktree ` field is the main
    // worktree. (The project *key* comes from rev-parse, not this listing, so
    // this only ever affects the derived repo_root/label, never the identity.)
    let listing = git(cwd, &["worktree", "list", "--porcelain", "-z"])?;
    listing
        .split('\0')
        .find_map(|field| field.strip_prefix("worktree "))
        .map(PathBuf::from)
}

/// Warn — at most once per process — that git's `--path-format=absolute` query
/// failed inside a confirmed git repository, which almost always means git is
/// older than 2.31 (the flag's introduction). On such a host no project ever
/// auto-registers, and without this the failure is silent (detection just keeps
/// returning `None`, indistinguishable from a non-git directory). The one-time
/// guard keeps it off the per-session hot path's log while still making a too-old
/// git diagnosable; it never changes behavior — detection stays non-fatal.
fn warn_git_too_old() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        warn!(
            "git `rev-parse --path-format=absolute` failed inside a git work tree; \
             git is likely older than 2.31 — projects will not auto-register on this \
             host until git is upgraded (detection stays non-fatal; sessions still start)"
        );
    }
}

/// Run `git -C <cwd> <args>` bounded by [`DETECT_GIT_TIMEOUT`], returning trimmed
/// stdout on a zero exit and `None` on any failure (see [`run_bounded`]). Shared
/// with [`crate::project`] so `project show`'s live `git worktree list` is bound
/// by the same hot-path discipline as detection.
pub(crate) fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(cwd).args(args);
    run_bounded(command, DETECT_GIT_TIMEOUT)
}

/// Spawn `command` and return its trimmed stdout on a zero exit within `timeout`,
/// else `None` (spawn error, non-zero exit, empty output, timeout, or a drain
/// I/O error). Forces `stdin`/`stderr` closed and `stdout` piped.
///
/// stdout is drained on a **dedicated reader thread** that runs concurrently with
/// the wait loop, so a child that writes more than the OS pipe buffer (~64 KiB on
/// Linux) — `git worktree list --porcelain` grows with the worktree count and can
/// exceed it on a busy repo — can never block on its write and wedge the wait into
/// the timeout. On timeout the child is killed (the detection commands do not fork,
/// so the direct kill leaves nothing behind — no process-group dance, no `unsafe`),
/// which closes the pipe and lets the reader finish; the reader is then joined.
fn run_bounded(mut command: Command, timeout: Duration) -> Option<String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| debug!(error = %err, "failed to spawn detection subprocess"))
        .ok()?;

    // Take the pipe and drain it on its own thread; reading concurrently with the
    // wait is what prevents the fill-the-pipe deadlock.
    let mut stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        out
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    debug!("detection subprocess timed out; terminated");
                    let _ = child.kill();
                    let _ = child.wait();
                    // The kill closed the write end; the reader now hits EOF and
                    // exits, so this join cannot hang on a runaway child.
                    let _ = reader.join();
                    return None;
                }
                thread::sleep(DETECT_POLL_INTERVAL);
            }
            Err(err) => {
                debug!(error = %err, "failed to wait on detection subprocess");
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
        }
    };

    // The child has exited, so its write end is closed and the reader returns
    // promptly; a panicked reader (it cannot panic here) degrades to empty.
    let out = reader.join().unwrap_or_default();
    if !status.success() {
        return None;
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use super::{detect, project_id, run_bounded};

    /// A fresh, unique temp directory (mirrors the worktree tests' convention).
    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "pohunek-detect-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn git_ok(dir: &Path, args: &[&str]) {
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

    fn git_stdout(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// Init a repo on branch `main` with one commit, returning its dir.
    fn init_repo(tag: &str) -> PathBuf {
        let dir = unique_dir(tag);
        let init = Command::new("git")
            .args(["-c", "init.defaultBranch=main", "init", "-q"])
            .arg(&dir)
            .output()
            .expect("git init");
        assert!(init.status.success(), "git init failed");
        git_ok(&dir, &["config", "user.email", "test@example.com"]);
        git_ok(&dir, &["config", "user.name", "Test"]);
        git_ok(&dir, &["config", "commit.gpgsign", "false"]);
        fs::write(dir.join("README.md"), "init\n").expect("write README");
        git_ok(&dir, &["add", "."]);
        git_ok(&dir, &["commit", "-q", "-m", "init"]);
        dir
    }

    #[test]
    fn detects_main_checkout() {
        let repo = init_repo("main-checkout");
        let project = detect(&repo).expect("detect ok").expect("a project");

        assert!(!project.is_linked_worktree, "main checkout is not linked");
        assert!(!project.is_bare);
        assert_eq!(project.branch.as_deref(), Some("main"));
        assert_eq!(
            project.git_common_dir,
            fs::canonicalize(repo.join(".git")).expect("canonical .git")
        );
        let canonical_repo = fs::canonicalize(&repo).expect("canonical repo");
        assert_eq!(project.repo_root, canonical_repo);
        assert_eq!(project.checkout_path, canonical_repo);
    }

    #[test]
    fn detects_linked_worktree_sharing_the_common_dir() {
        let repo = init_repo("linked-main");
        let worktree = unique_dir("linked-wt").join("checkout");
        git_ok(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                worktree.to_str().expect("utf8 path"),
            ],
        );

        let main = detect(&repo).expect("detect main").expect("main project");
        let linked = detect(&worktree)
            .expect("detect linked")
            .expect("linked project");

        assert!(linked.is_linked_worktree, "the worktree must be linked");
        assert!(!main.is_linked_worktree);
        // Both share one identity key — the whole point of the design.
        assert_eq!(
            linked.git_common_dir, main.git_common_dir,
            "main and worktree must collapse to one project key"
        );
        assert_eq!(linked.branch.as_deref(), Some("feature"));
        // repo_root points back at the main checkout; checkout_path is this tree.
        assert_eq!(linked.repo_root, main.repo_root);
        assert_eq!(
            linked.checkout_path,
            fs::canonicalize(&worktree).expect("canonical worktree")
        );
    }

    #[test]
    fn non_git_directory_is_no_project() {
        let dir = unique_dir("non-git");
        assert!(
            detect(&dir).expect("detect ok").is_none(),
            "a plain directory is not a project"
        );
    }

    #[test]
    fn detached_head_has_no_branch_but_is_a_project() {
        let repo = init_repo("detached");
        let head = git_stdout(&repo, &["rev-parse", "HEAD"]);
        git_ok(&repo, &["checkout", "-q", "--detach", &head]);

        let project = detect(&repo).expect("detect ok").expect("a project");
        assert_eq!(project.branch, None, "detached HEAD has no branch");
        assert!(!project.is_linked_worktree);
    }

    #[test]
    fn symlinked_cwd_canonicalizes_to_the_same_key() {
        let repo = init_repo("symlink");
        let link = unique_dir("symlink-link").join("alias");
        std::os::unix::fs::symlink(&repo, &link).expect("create symlink");

        let direct = detect(&repo).expect("detect direct").expect("project");
        let via_link = detect(&link).expect("detect via link").expect("project");

        assert_eq!(
            direct.git_common_dir, via_link.git_common_dir,
            "a symlinked path must resolve to the same project key"
        );
        assert_eq!(direct.repo_root, via_link.repo_root);
    }

    #[test]
    fn detects_bare_repository() {
        let parent = unique_dir("bare");
        let bare = parent.join("repo.git");
        let init = Command::new("git")
            .args(["-c", "init.defaultBranch=main", "init", "--bare", "-q"])
            .arg(&bare)
            .output()
            .expect("git init --bare");
        assert!(init.status.success(), "git init --bare failed");

        let project = detect(&bare).expect("detect ok").expect("a bare project");
        assert!(project.is_bare, "a bare repo must be flagged");
        assert_eq!(
            project.git_common_dir,
            fs::canonicalize(&bare).expect("canonical bare repo")
        );
        // No working tree: repo_root falls back to the repository itself.
        assert_eq!(project.repo_root, project.git_common_dir);
    }

    #[test]
    fn run_bounded_drains_output_larger_than_the_pipe_buffer() {
        // Regression for the drain-after-exit deadlock: a child that writes far
        // more than the ~64 KiB OS pipe buffer must still complete, because the
        // reader thread drains stdout concurrently with the wait — it must NOT
        // stall into the timeout and return None (which is what `git worktree
        // list --porcelain` on a busy repo would have done before the fix).
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("yes pohunek | head -c 200000");
        let out = run_bounded(cmd, Duration::from_secs(10)).expect("large output drains, not None");
        assert!(
            out.len() > 65536,
            "must drain past the pipe buffer; got {} bytes",
            out.len()
        );
    }

    #[test]
    fn run_bounded_times_out_and_kills_a_slow_child() {
        // A wedged subprocess must be bounded by the timeout and killed, not
        // waited on — detection runs on the session-start hot path.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 30");
        let start = Instant::now();
        let out = run_bounded(cmd, Duration::from_millis(200));
        assert!(out.is_none(), "a timed-out call yields None");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "must return promptly after the timeout, not wait for the child"
        );
    }

    #[test]
    fn project_id_is_stable_prefixed_and_path_specific() {
        let id = project_id(Path::new("/home/u/Code/ui/.git"));
        assert!(id.starts_with("p-"), "id must be prefixed: {id}");
        assert_eq!(id.len(), "p-".len() + 8, "id is p- plus 8 hex digits: {id}");
        assert!(
            id["p-".len()..].chars().all(|c| c.is_ascii_hexdigit()),
            "id body must be hex: {id}"
        );
        // Deterministic for one path, distinct across paths.
        assert_eq!(id, project_id(Path::new("/home/u/Code/ui/.git")));
        assert_ne!(id, project_id(Path::new("/home/u/Code/api/.git")));
    }
}
