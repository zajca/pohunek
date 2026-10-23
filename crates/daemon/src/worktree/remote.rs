//! Worktrees built from `origin/<branch>` (`session.new` `branch_source: remote`).
//!
//! The plan is made first and changes nothing but a private fetch ref: fetch the
//! branch, compare it with the local branch of the same name, and refuse a
//! diverged branch unless the caller asked for a backup and reset. The plan is
//! applied after the pre-create hook, with compare-and-swap ref updates, so a
//! branch that moved meanwhile is never overwritten. No local commit becomes
//! unreachable without a `backup/<branch>-<short-sha>` ref.

// Rust guideline compliant 2026-06-26

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use protocol::{
    ErrorClass, OnDiverged, ProtocolError, SessionWarning, SessionWarningKind, WorktreeSource,
};

use super::{
    admin_lock, branch_exists, branch_in_use_error, error, fetch_into_ref, fetch_ref_name,
    git_capture, git_command, git_output, has_origin, lock_within, output_failure_message,
    run_output_bounded, worktree_add_error, worktree_add_existing, worktree_add_new, FetchedBase,
    GIT_COMMAND_TIMEOUT,
};

/// Name of the only remote a remote-mode branch is taken from.
const REMOTE: &str = "origin";

/// Namespace of the refs that keep the local head of a reset branch.
const BACKUP_NAMESPACE: &str = "backup";

/// Hex digits of the local head in a backup ref name. Twelve keep two backups of
/// one branch distinct for any realistic history while the name stays typeable.
const BACKUP_SHA_LEN: usize = 12;

/// Reflog message of the ref updates made for a remote-mode branch.
const REFLOG_MESSAGE: &str = "pohunek: session new from remote branch";

/// What applying the plan does to the local branch before the worktree is added.
#[derive(Debug)]
enum Action {
    /// No local branch: create it at the fetched commit.
    Create,
    /// The local branch is the fetched commit.
    UseLocal,
    /// The local branch is behind the fetched commit: fast-forward it.
    FastForward { from: String },
    /// The local branch is ahead of the fetched commit: use it as it is.
    Ahead { local: String },
    /// The local branch diverged: keep its head as `backup`, then reset it.
    Reset { local: String, backup: String },
}

/// The decision for one remote-mode branch, holding the fetched commit alive.
#[derive(Debug)]
pub(super) struct RemotePlan {
    /// The fetched `origin/<branch>`; its ref is deleted when the plan drops.
    pub(super) fetched: FetchedBase,
    action: Action,
}

impl RemotePlan {
    /// The warning the plan raises, if any.
    pub(super) fn warning(&self, branch: &str) -> Option<SessionWarning> {
        let Action::Ahead { local } = &self.action else {
            return None;
        };
        Some(SessionWarning {
            kind: SessionWarningKind::BranchAheadOfRemote,
            message: format!(
                "Branch {branch:?} is ahead of {REMOTE}/{branch}, so the worktree starts at the local head, which holds commits that are not on the remote."
            ),
            detail: Some(format!("local={local} remote={}", self.fetched.commit)),
        })
    }

    /// The commit the worktree is built from, as reported to the caller.
    pub(super) fn source(&self, branch: &str) -> WorktreeSource {
        let remote = format!("{REMOTE}/{branch}");
        let remote_commit = self.fetched.commit.clone();
        let (source_ref, commit) = match &self.action {
            Action::Ahead { local } => (format!("refs/heads/{branch}"), local.clone()),
            _ => (remote, remote_commit),
        };
        WorktreeSource {
            source_ref,
            commit,
            fast_forwarded_from: match &self.action {
                Action::FastForward { from } => Some(from.clone()),
                _ => None,
            },
            backup_ref: match &self.action {
                Action::Reset { backup, .. } => Some(backup.clone()),
                _ => None,
            },
        }
    }
}

/// Fetch `origin/<branch>` and decide how the worktree gets its branch.
///
/// # Errors
///
/// `remote_branch_not_found` when there is no `origin` or it has no such
/// branch, `branch_fetch_failed` when the fetch fails otherwise,
/// `branch_diverged` when the local branch diverged and no reset was requested,
/// `backup_ref_exists` when the backup name is taken, and
/// `worktree_branch_in_use` when the branch would have to move while another
/// worktree has it checked out.
pub(super) fn plan_remote_branch(
    repository: &Path,
    branch: &str,
    on_diverged: Option<OnDiverged>,
) -> Result<RemotePlan, ProtocolError> {
    check_branch_name(repository, branch)?;
    if !has_origin(repository) {
        return Err(not_found(branch, "the repository has no origin remote"));
    }
    let fetched = fetch_remote_branch(repository, branch)?;
    let remote = fetched.commit.clone();
    let local_ref = format!("refs/heads/{branch}");
    let action = match branch_exists(repository, branch) {
        Ok(false) => Action::Create,
        Ok(true) => {
            let local = rev_parse_commit(repository, &local_ref).map_err(|message| {
                error(
                    ErrorClass::Runtime,
                    "worktree_add_failed",
                    format!("could not resolve local branch {branch:?}: {message}"),
                    None,
                )
            })?;
            relate(repository, branch, local, &remote, on_diverged)?
        }
        Err(message) => {
            return Err(error(
                ErrorClass::Runtime,
                "worktree_add_failed",
                format!("could not verify local branch {branch:?}: {message}"),
                None,
            ));
        }
    };
    // Refusing here, before the pre-create hook, keeps a doomed request free of
    // hook side effects; `move_branch` repeats the check under the admin lock,
    // which excludes the daemon's own concurrent `git worktree add` (the lock is
    // in-process, so a `git worktree add` run in a terminal is not excluded).
    if matches!(action, Action::FastForward { .. } | Action::Reset { .. }) {
        refuse_held_branch(repository, branch)?;
    }
    Ok(RemotePlan { fetched, action })
}

/// How the local head `local` relates to the fetched `remote`.
fn relate(
    repository: &Path,
    branch: &str,
    local: String,
    remote: &str,
    on_diverged: Option<OnDiverged>,
) -> Result<Action, ProtocolError> {
    if local == remote {
        return Ok(Action::UseLocal);
    }
    if is_ancestor(repository, &local, remote)? {
        return Ok(Action::FastForward { from: local });
    }
    if is_ancestor(repository, remote, &local)? {
        return Ok(Action::Ahead { local });
    }
    let short = &local[..BACKUP_SHA_LEN.min(local.len())];
    let backup = format!("{BACKUP_NAMESPACE}/{branch}-{short}");
    let merge_base = merge_base(repository, &local, remote)?;
    match on_diverged {
        None => Err(error(
            ErrorClass::Runtime,
            "branch_diverged",
            format!(
                "branch {branch:?} has diverged from {REMOTE}/{branch}: local={local} remote={remote} merge_base={merge_base}"
            ),
            Some(format!(
                "reconcile the branches, or pass --reset-diverged to keep the local head as {backup} and reset {branch:?} to the remote head"
            )),
        )),
        Some(OnDiverged::BackupReset) => {
            // A backup that already holds the local head is reused by the update.
            let holds_local =
                ref_head(repository, &format!("refs/heads/{backup}"))?.as_deref() == Some(&local);
            if !holds_local {
                if let Some(why) = backup_obstruction(repository, &backup)? {
                    return Err(backup_exists(&backup, &why));
                }
            }
            Ok(Action::Reset { local, backup })
        }
    }
}

/// Fetch `refs/heads/<branch>` from `origin` into a private ref.
///
/// A failed fetch is `remote_branch_not_found` only when `git ls-remote`
/// proves the branch absent; any other failure, including an unreachable
/// remote, is `branch_fetch_failed`.
fn fetch_remote_branch(repository: &Path, branch: &str) -> Result<FetchedBase, ProtocolError> {
    let fetch_ref = fetch_ref_name().map_err(|message| fetch_failed(branch, &message))?;
    let source = format!("refs/heads/{branch}");
    match fetch_into_ref(repository, &source, &fetch_ref) {
        Ok(fetched) => Ok(fetched),
        // A fetch that ran out of time already spent its budget: asking the
        // same remote again would only double the wait.
        Err(message) if message.contains("timed out") => Err(fetch_failed(branch, &message)),
        Err(message) => match remote_has_branch(repository, &source) {
            Ok(false) => Err(not_found(
                branch,
                &format!("{REMOTE} has no branch {branch:?}"),
            )),
            Ok(true) | Err(_) => Err(fetch_failed(branch, &message)),
        },
    }
}

/// Whether `origin` advertises exactly `source` (a full `refs/heads/...` name).
fn remote_has_branch(repository: &Path, source: &str) -> Result<bool, String> {
    let mut cmd = git_command(repository)?;
    cmd.args(["ls-remote", "--exit-code", "--heads", REMOTE])
        .arg("--end-of-options")
        .arg(source);
    let output = run_output_bounded(cmd, GIT_COMMAND_TIMEOUT)?;
    match output.status.code() {
        Some(0) => Ok(String::from_utf8_lossy(&output.stdout).lines().any(|line| {
            line.split_once('\t')
                .is_some_and(|(_, name)| name == source)
        })),
        // `--exit-code` reports "no matching refs" as 2.
        Some(2) => Ok(false),
        _ => Err(output_failure_message(&output)),
    }
}

/// Apply the plan's ref changes and add the worktree at `path`.
///
/// # Errors
///
/// The `worktree_add_failed` / `worktree_branch_in_use` of the add, or the
/// error of the ref update: a lost compare-and-swap is `worktree_add_failed`
/// naming a concurrent move of the local branch, and an unusable backup name is
/// `backup_ref_exists`.
pub(super) fn create_remote_worktree(
    repository: &Path,
    path: &Path,
    branch: &str,
    plan: &RemotePlan,
) -> Result<(), ProtocolError> {
    let remote = &plan.fetched.commit;
    match &plan.action {
        Action::Create => worktree_add_new(repository, path, branch, remote)
            .map_err(|message| worktree_add_error(branch, &message)),
        Action::UseLocal | Action::Ahead { .. } => worktree_add_existing(repository, path, branch)
            .map_err(|message| worktree_add_error(branch, &message)),
        Action::FastForward { from } => {
            move_branch(repository, branch, from, remote, None)?;
            worktree_add_existing(repository, path, branch)
                .map_err(|message| worktree_add_error(branch, &message))
        }
        Action::Reset { local, backup } => {
            move_branch(repository, branch, local, remote, Some(backup))?;
            worktree_add_existing(repository, path, branch).map_err(|message| {
                note_backup(worktree_add_error(branch, &message), branch, backup)
            })
        }
    }
}

/// `err` plus the name of the backup ref, for a failure after the branch was
/// reset: the old head is only reachable through that ref.
pub(crate) fn note_backup(mut err: ProtocolError, branch: &str, backup: &str) -> ProtocolError {
    err.msg = format!(
        "{}; branch {branch:?} was already reset to the remote head and its previous head is kept as {backup:?}",
        err.msg
    );
    err
}

/// Fail when the worktree at `path` did not start at `expected`.
///
/// `UseLocal` and `Ahead` have no compare-and-swap, so a branch that moved
/// between the plan and the checkout (a pre-create hook, a concurrent session)
/// would otherwise be reported as the planned commit.
///
/// # Errors
///
/// `worktree_add_failed` naming the commit the worktree is at.
pub(super) fn verify_start(
    path: &Path,
    branch: &str,
    source: &WorktreeSource,
) -> Result<(), ProtocolError> {
    let expected = source.commit.as_str();
    let err = match git_capture(path, &["rev-parse", "--verify", "HEAD"]) {
        Ok(head) if head == expected => return Ok(()),
        Ok(head) => error(
            ErrorClass::Runtime,
            "worktree_add_failed",
            format!(
                "local branch {branch:?} moved concurrently; the worktree is at {head} instead of {expected}"
            ),
            Some("retry the session".to_owned()),
        ),
        Err(message) => inspect_error(&message),
    };
    Err(match &source.backup_ref {
        Some(backup) => note_backup(err, branch, backup),
        None => err,
    })
}

/// Move `refs/heads/<branch>` from `old` to `new` while holding the repository's
/// worktree admin lock, first keeping `old` as `refs/heads/<backup>` when asked.
///
/// The lock keeps the daemon's own concurrent `git worktree add` from checking
/// the branch out between the in-use check and the update; it is in-process, so
/// a `git worktree add` run in a terminal is not excluded. A failed update is classified by the
/// refs it left behind ([`settle`]), never by the failure itself: a transaction
/// that timed out after committing, or one a concurrent identical request
/// applied first, counts as applied.
fn move_branch(
    repository: &Path,
    branch: &str,
    old: &str,
    new: &str,
    backup: Option<&String>,
) -> Result<(), ProtocolError> {
    let lock = admin_lock(repository).map_err(|message| inspect_error(&message))?;
    let _guard =
        lock_within(&lock, GIT_COMMAND_TIMEOUT).map_err(|message| inspect_error(&message))?;
    refuse_held_branch(repository, branch)?;
    let moves = Move {
        branch_ref: format!("refs/heads/{branch}"),
        backup_ref: backup.map(|name| format!("refs/heads/{name}")),
        branch,
        old,
        new,
    };
    // One transaction: the backup (which must not exist yet) and the compare-and-
    // swap reset both apply or neither does.
    let Err(message) = update_refs(repository, &moves.script(true)) else {
        return Ok(());
    };
    match settle(repository, &moves, false)? {
        Settled::Applied => Ok(()),
        Settled::BackupHoldsOld => {
            // A concurrent request already made the backup of this very head;
            // the reset only has to verify it.
            let Err(message) = update_refs(repository, &moves.script(false)) else {
                return Ok(());
            };
            match settle(repository, &moves, true)? {
                Settled::Applied => Ok(()),
                Settled::BackupHoldsOld | Settled::Failed => Err(moves.update_failed(&message)),
            }
        }
        Settled::Failed => Err(moves.update_failed(&message)),
    }
}

/// The ref update of [`move_branch`].
struct Move<'a> {
    branch_ref: String,
    backup_ref: Option<String>,
    branch: &'a str,
    old: &'a str,
    new: &'a str,
}

impl Move<'_> {
    /// The `update-ref --stdin` script: create the backup (or verify the one that
    /// already holds `old`), then swap the branch.
    fn script(&self, create_backup: bool) -> String {
        let mut lines = vec!["start".to_owned()];
        if let Some(backup_ref) = &self.backup_ref {
            let verb = if create_backup { "create" } else { "verify" };
            lines.push(format!("{verb} {backup_ref} {}", self.old));
        }
        lines.push(format!(
            "update {} {} {}",
            self.branch_ref, self.new, self.old
        ));
        lines.extend(["prepare".to_owned(), "commit".to_owned()]);
        lines.join("\n") + "\n"
    }

    fn update_failed(&self, message: &str) -> ProtocolError {
        error(
            ErrorClass::Runtime,
            "worktree_add_failed",
            format!("could not update local branch {:?}: {message}", self.branch),
            None,
        )
    }
}

/// What a failed update left behind.
enum Settled {
    /// The branch is at `new` and the backup (if any) holds `old`.
    Applied,
    /// The branch is still at `old` and the requested backup already holds it.
    BackupHoldsOld,
    /// Nothing was applied (the error is built by the caller or returned).
    Failed,
}

/// Classify a failed update by the refs it left.
///
/// A backup that holds `old` is never reported as an obstruction and never
/// advised to be deleted: it may be the only ref that still reaches `old`.
///
/// # Errors
///
/// `backup_ref_exists` for a backup name that is taken by something else while
/// the branch is still at `old`, and `worktree_add_failed` naming a concurrent
/// move (and the backup holding `old`, if any) for any other position.
fn settle(repository: &Path, moves: &Move<'_>, retried: bool) -> Result<Settled, ProtocolError> {
    // A ref that cannot be read is an error, not an absent ref: the update may
    // have committed, so the old head is named for recovery.
    let unreadable = |mut err: ProtocolError| {
        err.msg = format!(
            "{}; the previous head of {:?} was {}{}",
            err.msg,
            moves.branch,
            moves.old,
            moves
                .backup_ref
                .as_deref()
                .map_or_else(String::new, |backup| format!(
                    ", kept as {:?} if the update committed",
                    backup.trim_start_matches("refs/heads/")
                ))
        );
        err
    };
    let head = ref_head(repository, &moves.branch_ref).map_err(unreadable)?;
    let backup_holds_old = match &moves.backup_ref {
        Some(backup) => {
            ref_head(repository, backup).map_err(unreadable)?.as_deref() == Some(moves.old)
        }
        None => false,
    };
    if head.as_deref() == Some(moves.new) && (moves.backup_ref.is_none() || backup_holds_old) {
        return Ok(Settled::Applied);
    }
    if head.as_deref() == Some(moves.old) {
        if backup_holds_old && !retried {
            return Ok(Settled::BackupHoldsOld);
        }
        if let Some(backup) = &moves.backup_ref {
            let name = backup.trim_start_matches("refs/heads/");
            if !backup_holds_old {
                if let Some(why) = backup_obstruction(repository, name)? {
                    return Err(backup_exists(name, &why));
                }
            }
        }
        return Ok(Settled::Failed);
    }
    let kept = match &moves.backup_ref {
        Some(backup) if backup_holds_old => format!(
            "; the local head {} is kept as {:?}",
            moves.old,
            backup.trim_start_matches("refs/heads/")
        ),
        Some(_) if head.as_deref() == Some(moves.new) => format!(
            "; it is at {} but the backup does not hold {}, the previous head",
            moves.new, moves.old
        ),
        _ => "; no backup or reset was applied".to_owned(),
    };
    Err(error(
        ErrorClass::Runtime,
        "worktree_add_failed",
        format!("local branch {:?} moved concurrently{kept}", moves.branch),
        Some("retry the session".to_owned()),
    ))
}

/// Apply a `git update-ref --stdin` script, bounded by [`GIT_COMMAND_TIMEOUT`].
fn update_refs(repository: &Path, script: &str) -> Result<(), String> {
    let mut cmd = git_command(repository)?;
    cmd.args(["update-ref", "-m", REFLOG_MESSAGE, "--stdin"]);
    let captured =
        git_output::capture_with_input(cmd, GIT_COMMAND_TIMEOUT, || {}, Some(script.as_bytes()))?;
    if captured.output.status.success() {
        Ok(())
    } else {
        Err(output_failure_message(&captured.output))
    }
}

/// Refuse with `worktree_branch_in_use` when another worktree holds `branch`.
fn refuse_held_branch(repository: &Path, branch: &str) -> Result<(), ProtocolError> {
    match branch_holder(repository, branch).map_err(|message| inspect_error(&message))? {
        None => Ok(()),
        Some(Holder::CheckedOut) => Err(branch_in_use_error(branch)),
        Some(Holder::Operation(operation)) => Err(error(
            ErrorClass::Runtime,
            "worktree_branch_in_use",
            format!(
                "branch {branch:?} is being {} in another worktree",
                operation.participle()
            ),
            Some(format!(
                "finish or abort the {} there, or use a different branch for this session",
                operation.noun()
            )),
        )),
    }
}

/// How another worktree holds a branch.
#[derive(Debug, PartialEq, Eq)]
enum Holder {
    /// The branch is the worktree's checked-out HEAD.
    CheckedOut,
    /// The worktree's HEAD is detached while a rebase or bisect that started on
    /// the branch is in progress.
    Operation(Operation),
}

/// A multi-step git operation that leaves HEAD detached and returns to its
/// branch when it ends.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Operation {
    Rebase,
    Bisect,
}

impl Operation {
    fn noun(self) -> &'static str {
        match self {
            Self::Rebase => "rebase",
            Self::Bisect => "bisect",
        }
    }

    fn participle(self) -> &'static str {
        match self {
            Self::Rebase => "rebased",
            Self::Bisect => "bisected",
        }
    }
}

/// What holds `branch` in any worktree of the repository, including the main
/// checkout.
///
/// `git worktree list` reports a worktree in the middle of a rebase or bisect
/// as detached, so the per-worktree state files are read as well, the way git's
/// own branch-in-use check does. `git update-ref` has no such check.
fn branch_holder(repository: &Path, branch: &str) -> Result<Option<Holder>, String> {
    let listing = git_capture(repository, &["worktree", "list", "--porcelain"])?;
    let wanted = format!("refs/heads/{branch}");
    if listing.lines().any(|line| {
        line.strip_prefix("branch ")
            .is_some_and(|name| name == wanted)
    }) {
        return Ok(Some(Holder::CheckedOut));
    }
    let common = PathBuf::from(git_capture(
        repository,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?);
    let mut git_dirs = vec![common.clone()];
    match fs::read_dir(common.join("worktrees")) {
        Ok(entries) => {
            for entry in entries {
                let path = entry
                    .map_err(|err| format!("cannot list the worktree admin directories: {err}"))?
                    .path();
                if path.is_dir() {
                    git_dirs.push(path);
                }
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(format!("cannot list the worktree admin directories: {err}")),
    }
    for git_dir in git_dirs {
        if let Some(operation) = operation_on_branch(&git_dir, branch)? {
            return Ok(Some(Holder::Operation(operation)));
        }
    }
    Ok(None)
}

/// The rebase or bisect in progress in the worktree whose git directory is
/// `git_dir`, if it started on `branch`.
///
/// A `rebase-apply` directory with an `applying` file is `git am`, not a rebase.
fn operation_on_branch(git_dir: &Path, branch: &str) -> Result<Option<Operation>, String> {
    let rebase_head = if git_dir.join("rebase-apply").exists() {
        if git_dir.join("rebase-apply/applying").exists() {
            None
        } else {
            read_state(&git_dir.join("rebase-apply/head-name"))?
        }
    } else {
        read_state(&git_dir.join("rebase-merge/head-name"))?
    };
    if rebase_head
        .as_deref()
        .is_some_and(|name| name.strip_prefix("refs/heads/").unwrap_or(name) == branch)
    {
        return Ok(Some(Operation::Rebase));
    }
    // `rebase --update-refs` moves the other branches of a stack when it ends;
    // each record of the file is the ref name followed by its two object ids.
    let wanted = format!("refs/heads/{branch}");
    if read_state(&git_dir.join("rebase-merge/update-refs"))?
        .is_some_and(|refs| refs.lines().any(|line| line == wanted))
    {
        return Ok(Some(Operation::Rebase));
    }
    if git_dir.join("BISECT_LOG").exists()
        && read_state(&git_dir.join("BISECT_START"))?.as_deref() == Some(branch)
    {
        return Ok(Some(Operation::Bisect));
    }
    Ok(None)
}

/// The trimmed content of a git state file, or `None` when it does not exist.
/// A file that exists but cannot be read is an error: it may name the branch.
fn read_state(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content.trim().to_owned())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("cannot read {}: {err}", path.display())),
    }
}

/// Reject a name git does not accept as a branch before any remote work.
fn check_branch_name(repository: &Path, branch: &str) -> Result<(), ProtocolError> {
    let mut cmd = git_command(repository).map_err(|message| inspect_error(&message))?;
    cmd.args(["check-ref-format", "--branch", branch]);
    let output =
        run_output_bounded(cmd, GIT_COMMAND_TIMEOUT).map_err(|message| inspect_error(&message))?;
    // `--branch` also expands shorthands such as `@{-1}` to the branch they
    // name and prints the result; only a literal branch name is accepted.
    if output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == branch {
        return Ok(());
    }
    Err(error(
        ErrorClass::Runtime,
        "invalid_branch",
        format!("branch {branch:?} is not a valid git branch name"),
        None,
    ))
}

/// The best common ancestor of `local` and `remote`, or `none` for unrelated
/// histories.
fn merge_base(repository: &Path, local: &str, remote: &str) -> Result<String, ProtocolError> {
    let mut cmd = git_command(repository).map_err(|message| inspect_error(&message))?;
    cmd.args(["merge-base", local, remote]);
    let output =
        run_output_bounded(cmd, GIT_COMMAND_TIMEOUT).map_err(|message| inspect_error(&message))?;
    match output.status.code() {
        Some(0) => Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned()),
        // Exit 1 is "no common ancestor".
        Some(1) => Ok("none".to_owned()),
        _ => Err(inspect_error(&output_failure_message(&output))),
    }
}

/// Whether `ancestor` is an ancestor of (or equal to) `descendant`.
fn is_ancestor(repository: &Path, ancestor: &str, descendant: &str) -> Result<bool, ProtocolError> {
    let mut cmd = git_command(repository).map_err(|message| inspect_error(&message))?;
    cmd.args(["merge-base", "--is-ancestor", ancestor, descendant]);
    let output =
        run_output_bounded(cmd, GIT_COMMAND_TIMEOUT).map_err(|message| inspect_error(&message))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(inspect_error(&output_failure_message(&output))),
    }
}

/// Why `refs/heads/<backup>` cannot be created, if it cannot: the ref exists, or
/// a branch named like one of its parent directories does (git cannot hold
/// `backup` and `backup/<name>` at once).
fn backup_obstruction(repository: &Path, backup: &str) -> Result<Option<String>, ProtocolError> {
    if ref_exists(repository, &format!("refs/heads/{backup}"))? {
        return Ok(Some("the ref already exists".to_owned()));
    }
    let below = git_capture(
        repository,
        &[
            "for-each-ref",
            "--count=1",
            "--format=%(refname)",
            &format!("refs/heads/{backup}/"),
        ],
    )
    .map_err(|message| inspect_error(&message))?;
    if !below.is_empty() {
        return Ok(Some(format!("the branch {below:?} lies below that name")));
    }
    let mut prefix = String::new();
    let mut parts = backup.split('/').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            break;
        }
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(part);
        if ref_exists(repository, &format!("refs/heads/{prefix}"))? {
            return Ok(Some(format!("the branch {prefix:?} blocks that name")));
        }
    }
    Ok(None)
}

fn ref_exists(repository: &Path, name: &str) -> Result<bool, ProtocolError> {
    let mut cmd = git_command(repository).map_err(|message| inspect_error(&message))?;
    cmd.args(["rev-parse", "--verify", "--quiet", name]);
    let output =
        run_output_bounded(cmd, GIT_COMMAND_TIMEOUT).map_err(|message| inspect_error(&message))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(inspect_error(&output_failure_message(&output))),
    }
}

/// The commit `name` points to, or `None` when the ref does not exist.
fn ref_head(repository: &Path, name: &str) -> Result<Option<String>, ProtocolError> {
    let mut cmd = git_command(repository).map_err(|message| inspect_error(&message))?;
    cmd.args([
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("{name}^{{commit}}"),
    ]);
    let output =
        run_output_bounded(cmd, GIT_COMMAND_TIMEOUT).map_err(|message| inspect_error(&message))?;
    match output.status.code() {
        Some(0) => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        )),
        Some(1) => Ok(None),
        _ => Err(inspect_error(&output_failure_message(&output))),
    }
}

fn rev_parse_commit(repository: &Path, name: &str) -> Result<String, String> {
    git_capture(
        repository,
        &["rev-parse", "--verify", &format!("{name}^{{commit}}")],
    )
}

/// A git step of the plan or its application that could not run.
fn inspect_error(message: &str) -> ProtocolError {
    error(
        ErrorClass::Runtime,
        "worktree_add_failed",
        format!("could not inspect the repository for the remote branch: {message}"),
        None,
    )
}

fn not_found(branch: &str, why: &str) -> ProtocolError {
    error(
        ErrorClass::Runtime,
        "remote_branch_not_found",
        format!("cannot start from {REMOTE}/{branch}: {why}"),
        Some("push the branch first, or start without --from-remote".to_owned()),
    )
}

fn fetch_failed(branch: &str, message: &str) -> ProtocolError {
    error(
        ErrorClass::Runtime,
        "branch_fetch_failed",
        format!("fetching {branch:?} from {REMOTE} failed: {message}"),
        Some("check the remote, or start without --from-remote".to_owned()),
    )
}

fn backup_exists(backup: &str, why: &str) -> ProtocolError {
    error(
        ErrorClass::Runtime,
        "backup_ref_exists",
        format!("cannot keep the local head as {backup:?}: {why}"),
        Some("delete or rename that ref, then retry".to_owned()),
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{operation_on_branch, Operation};

    /// A git directory holding the given files (relative path, content).
    fn git_dir(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = pohunek_test_support::tempdir().expect("create a git directory");
        for (path, content) in files {
            let full = dir.path().join(path);
            fs::create_dir_all(full.parent().expect("parent")).expect("create the directory");
            fs::write(full, content).expect("write the state file");
        }
        dir
    }

    fn operation(files: &[(&str, &str)]) -> Option<Operation> {
        let dir = git_dir(files);
        operation_on_branch(dir.path(), "feat").expect("read the state")
    }

    #[test]
    fn an_interactive_or_merge_rebase_of_the_branch_is_detected() {
        let files = [("rebase-merge/head-name", "refs/heads/feat\n")];
        assert_eq!(operation(&files), Some(Operation::Rebase));
    }

    #[test]
    fn an_apply_rebase_of_the_branch_is_detected() {
        let files = [("rebase-apply/head-name", "refs/heads/feat\n")];
        assert_eq!(operation(&files), Some(Operation::Rebase));
    }

    #[test]
    fn a_rebase_of_another_or_a_detached_head_is_not_a_hold() {
        assert_eq!(
            operation(&[("rebase-merge/head-name", "refs/heads/other\n")]),
            None
        );
        assert_eq!(
            operation(&[("rebase-merge/head-name", "detached HEAD\n")]),
            None
        );
    }

    #[test]
    fn a_branch_a_rebase_will_update_is_held_by_that_rebase() {
        let files = [
            ("rebase-merge/head-name", "refs/heads/upper\n"),
            (
                "rebase-merge/update-refs",
                "refs/heads/feat\n1111111111111111111111111111111111111111\n2222222222222222222222222222222222222222\n",
            ),
        ];
        assert_eq!(operation(&files), Some(Operation::Rebase));
    }

    #[test]
    fn git_am_is_not_a_rebase_of_the_branch() {
        let files = [
            ("rebase-apply/applying", ""),
            ("rebase-apply/head-name", "refs/heads/feat\n"),
        ];
        assert_eq!(operation(&files), None);
    }

    #[test]
    fn a_bisect_started_on_the_branch_is_detected_only_while_it_runs() {
        assert_eq!(
            operation(&[
                ("BISECT_LOG", "git bisect start\n"),
                ("BISECT_START", "feat\n")
            ]),
            Some(Operation::Bisect)
        );
        assert_eq!(operation(&[("BISECT_START", "feat\n")]), None);
        assert_eq!(
            operation(&[("BISECT_LOG", ""), ("BISECT_START", "other\n")]),
            None
        );
    }

    #[test]
    fn a_clean_git_directory_holds_nothing() {
        assert_eq!(operation(&[]), None);
    }

    #[test]
    fn the_main_checkout_mid_rebase_holds_its_branch() {
        use super::{branch_holder, Holder};

        let repo = pohunek_test_support::tempdir().expect("create a repository");
        let git = |args: &[&str]| {
            pohunek_test_support::process_env::command("git")
                .arg("-C")
                .arg(repo.path())
                .args(["-c", "user.email=t@example.com", "-c", "user.name=T"])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .expect("run git")
        };
        let write = |content: &str| {
            fs::write(repo.path().join("clash.txt"), content).expect("write the clash file");
        };
        assert!(git(&["init", "-q", "-b", "main"]).status.success());
        git(&["commit", "-q", "--allow-empty", "-m", "base"]);
        git(&["checkout", "-q", "-b", "feat"]);
        write("feat\n");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "feat"]);
        git(&["checkout", "-q", "main"]);
        write("main\n");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "main"]);
        git(&["checkout", "-q", "feat"]);
        assert!(
            !git(&["rebase", "main"]).status.success(),
            "stops on the conflict"
        );

        let holder = branch_holder(repo.path(), "feat").expect("inspect");

        assert_eq!(holder, Some(Holder::Operation(Operation::Rebase)));
        assert_eq!(branch_holder(repo.path(), "main").expect("inspect"), None);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_state_file_is_an_error_not_an_absent_hold() {
        // A directory where the file belongs fails the read with something other
        // than "not found".
        let dir = git_dir(&[]);
        fs::create_dir_all(dir.path().join("rebase-merge/head-name")).expect("create the dir");
        operation_on_branch(dir.path(), "feat").expect_err("the read fails");
    }
}
