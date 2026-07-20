//! `session.diff` — worktree-vs-base unified diff computation.
//!
//! A pure read: resolves the session's worktree binding, computes a unified
//! diff of the worktree against `merge-base(base, HEAD)` for tracked changes,
//! appends each untracked file rendered as an added-file diff (via `git diff
//! --no-index`), and truncates the combined text at a file boundary to respect
//! [`protocol::MAX_SESSION_DIFF_BYTES`]. Git is always shelled out via
//! `std::process::Command` (mirrors `crate::worktree`); there is no git2/libgit2
//! dependency in this codebase and this module keeps it that way.

use std::path::Path;
use std::process::Output;

use protocol::{ErrorClass, ProtocolError, SessionDiffResult, SessionId, MAX_SESSION_DIFF_BYTES};

use super::{runtime_error, SessionRegistry};
use crate::store::Store;
use crate::worktree::{
    git_command, output_failure_message, run_output_bounded, validate_git_ref_arg, WorktreeManager,
    GIT_COMMAND_TIMEOUT,
};

impl SessionRegistry {
    /// Diff a session's worktree against its resolved base ref.
    ///
    /// Resolves `base` with precedence: the caller's explicit `base` (validated
    /// before it ever reaches a `git` argv), else the worktree binding's
    /// recorded base branch, else the repository's default branch. The
    /// returned [`SessionDiffResult::base`] echoes whichever ref was actually
    /// used.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`]: the registry's usual `session_not_found`
    /// for an unknown id, `session_no_worktree` when the session owns no
    /// worktree, the `validate_git_ref_arg` rejection for a hostile explicit
    /// `base`, `session_diff_base_unresolved` when `base` cannot be resolved to
    /// a merge-base against `HEAD`, or `session_diff_failed` for any other git
    /// subprocess failure.
    pub async fn diff(
        &self,
        id: &SessionId,
        base: Option<String>,
    ) -> Result<SessionDiffResult, ProtocolError> {
        if let Some(base) = base.as_deref() {
            validate_git_ref_arg(base, "base ref")?;
        }
        let info = self.inspect(id).await?;
        let Some(worktree) = info.worktree_path.clone() else {
            return Err(session_no_worktree(id));
        };
        let repository = info.repo.clone();
        let store = self.inner.store.clone();
        let session_id = id.0.clone();
        tokio::task::spawn_blocking(move || -> Result<SessionDiffResult, ProtocolError> {
            let resolved_base =
                resolve_base(base, store.as_deref(), &session_id, repository.as_deref())?;
            compute_session_diff(&worktree, &resolved_base)
        })
        .await
        .map_err(|err| {
            runtime_error(
                "session_diff_failed",
                format!("session diff task panicked: {err}"),
            )
        })?
    }
}

/// Resolve the base ref precedence: explicit param, else the recorded worktree
/// binding's base branch, else the repository's default branch.
///
/// Runs on the blocking thread `diff` spawns onto (store I/O and, in the
/// fallback case, a `git` subprocess via [`WorktreeManager::default_branch`]).
///
/// `pub(super)` so `session::tests` can exercise all three precedence branches
/// directly (including the repository-default fallback, which a live
/// `SessionRegistry` cannot reach: a worktree-bound session's binding always
/// carries a `base_branch`, since `WorktreeManager::bind` resolves and records
/// one — even absent an explicit `--base-branch` — before the worktree exists).
pub(super) fn resolve_base(
    explicit: Option<String>,
    store: Option<&Store>,
    session_id: &str,
    repository: Option<&Path>,
) -> Result<String, ProtocolError> {
    if let Some(base) = explicit {
        return Ok(base);
    }
    if let Some(store) = store {
        if let Some(binding) = store
            .find_worktree_for_session(session_id)
            .map_err(|err| session_diff_store_error(&err))?
        {
            return Ok(binding.base_branch);
        }
    }
    // A session only ever carries `worktree_path: Some(_)` alongside
    // `repo: Some(_)` — both are set together from the same `WorktreeBound`
    // wherever `SessionInfo` is constructed (`session::target`,
    // `session::mod::resolve_cwd_association`). Reaching here with `repository
    // == None` would mean that invariant broke elsewhere; `diff` runs inside
    // `spawn_blocking`, so a panic here is contained to a `JoinError`, not a
    // process crash.
    let repository = repository.expect(
        "SessionInfo.repo is always set alongside worktree_path when a session owns a worktree",
    );
    WorktreeManager::default_branch(repository)
}

/// Compute the full `session.diff` result for an already-resolved `base` ref
/// against a worktree on disk.
///
/// `pub(super)` so `session::tests` can drive the git-diff computation matrix
/// (modified/added/deleted/renamed/untracked/binary/truncation) directly
/// against a plain fixture repo, without needing a live session or the
/// `SessionRegistry` plumbing above it.
pub(super) fn compute_session_diff(
    worktree: &Path,
    base: &str,
) -> Result<SessionDiffResult, ProtocolError> {
    let merge_base_sha = merge_base(worktree, base)?;
    let mut combined = tracked_diff(worktree, &merge_base_sha)?;
    for path in untracked_files(worktree)? {
        combined.push_str(&untracked_diff(worktree, &path)?);
    }
    let (diff, truncated) = cap_to_budget(&combined);
    Ok(SessionDiffResult {
        diff,
        base: base.to_owned(),
        truncated,
    })
}

/// Resolve `merge-base(base, HEAD)` in `worktree`, as a trimmed commit sha.
///
/// `--end-of-options` forces `base` to be parsed positionally even though
/// [`validate_git_ref_arg`] already rejected a leading `-` — defense in depth,
/// matching `crate::worktree`'s own git invocations.
fn merge_base(worktree: &Path, base: &str) -> Result<String, ProtocolError> {
    let output = git_output(worktree, &["merge-base", "--end-of-options", base, "HEAD"])?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(session_diff_base_unresolved(
            base,
            &output_failure_message(&output),
        ))
    }
}

/// Unified diff of tracked changes in `worktree` against `merge_base_sha`.
///
/// `--find-renames` is passed explicitly so rename detection does not depend on
/// the ambient `diff.renames` git config (`git diff` does not enable it by
/// default). `git diff <commit>` exits `0` whether or not there are
/// differences; only a non-zero status is an error (e.g. an unreadable
/// object).
fn tracked_diff(worktree: &Path, merge_base_sha: &str) -> Result<String, ProtocolError> {
    let output = git_output(
        worktree,
        &[
            "diff",
            "--no-color",
            "--find-renames",
            "--end-of-options",
            merge_base_sha,
        ],
    )?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(session_diff_failed(&format!(
            "git diff against {merge_base_sha} failed: {}",
            output_failure_message(&output)
        )))
    }
}

/// Untracked file paths in `worktree`, relative to its root, sorted for a
/// deterministic diff ordering.
///
/// Parsed from `git status --porcelain=v1 --untracked-files=all -z`: NUL-
/// separated entries sidestep the quoting/escaping `git status` applies to
/// unusual filenames in its human-readable (non-`-z`) output. A rename/copy
/// entry (status `R`/`C`) carries an extra NUL-separated original-path field;
/// it is skipped so the following entry stays aligned, even though only `??`
/// (untracked) entries are collected here.
fn untracked_files(worktree: &Path) -> Result<Vec<String>, ProtocolError> {
    let output = git_output(
        worktree,
        &["status", "--porcelain=v1", "--untracked-files=all", "-z"],
    )?;
    if !output.status.success() {
        return Err(session_diff_failed(&format!(
            "git status failed: {}",
            output_failure_message(&output)
        )));
    }
    let mut paths = Vec::new();
    let mut entries = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty());
    while let Some(entry) = entries.next() {
        let text = String::from_utf8_lossy(entry);
        if let Some(path) = text.strip_prefix("?? ") {
            paths.push(path.to_owned());
            continue;
        }
        let code = text.as_bytes();
        if code.len() >= 2
            && (code[0] == b'R' || code[0] == b'C' || code[1] == b'R' || code[1] == b'C')
        {
            // Rename/copy entries carry an extra NUL-separated orig-path field;
            // consume it so the next `entries.next()` lands on the next entry.
            let _ = entries.next();
        }
    }
    paths.sort();
    Ok(paths)
}

/// Render one untracked file as an added-file diff via `git diff --no-index`.
///
/// `-- /dev/null <path>` forces both arguments to be parsed positionally, so a
/// maliciously named untracked file (e.g. one literally named `--upload-pack=x`)
/// cannot be misread as a flag. `--no-index` exits `1` when the two sides
/// differ (the expected case) and `0` only for a byte-identical (empty vs.
/// empty) comparison; both are success. `>= 2` is a genuine git failure.
/// Binary untracked files come back as git's own "Binary files ... differ"
/// stanza, with no special-casing needed here.
fn untracked_diff(worktree: &Path, relative_path: &str) -> Result<String, ProtocolError> {
    let output = git_output(
        worktree,
        &[
            "diff",
            "--no-color",
            "--no-index",
            "--",
            "/dev/null",
            relative_path,
        ],
    )?;
    match output.status.code() {
        Some(0 | 1) => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
        _ => Err(session_diff_failed(&format!(
            "git diff --no-index for untracked file {relative_path:?} failed: {}",
            output_failure_message(&output)
        ))),
    }
}

/// Run `git -C <worktree> <args>` bounded by [`GIT_COMMAND_TIMEOUT`], mapping a
/// spawn/wait/timeout failure to a typed `session_diff_failed` error. The
/// command's own exit status (success or not) is left to the caller — every
/// git operation `session::diff` runs has command-specific exit-code semantics
/// (see e.g. [`untracked_diff`]'s use of exit code 1 as success).
fn git_output(worktree: &Path, args: &[&str]) -> Result<Output, ProtocolError> {
    let mut cmd = git_command(worktree);
    cmd.args(args);
    run_output_bounded(cmd, GIT_COMMAND_TIMEOUT)
        .map_err(|message| session_diff_failed(&format!("git {}: {message}", args.join(" "))))
}

/// Split `text` into chunks at each line-start `"diff --git "` marker (the
/// first line of every per-file entry `git diff`/`git diff --no-index` emit,
/// tracked or untracked, text or binary), so [`cap_to_budget`] can decide
/// inclusion at a whole-file granularity.
fn diff_git_chunks(text: &str) -> Vec<&str> {
    const MARKER: &str = "diff --git ";
    let mut starts = Vec::new();
    if text.starts_with(MARKER) {
        starts.push(0);
    }
    let mut search_from = 0;
    while let Some(offset) = text[search_from..].find('\n') {
        let line_start = search_from + offset + 1;
        if text[line_start..].starts_with(MARKER) {
            starts.push(line_start);
        }
        search_from = line_start;
    }
    if starts.is_empty() {
        return if text.is_empty() {
            Vec::new()
        } else {
            vec![text]
        };
    }
    let mut chunks: Vec<&str> = starts.windows(2).map(|w| &text[w[0]..w[1]]).collect();
    if let Some(&last) = starts.last() {
        chunks.push(&text[last..]);
    }
    chunks
}

/// Truncate `text` at a whole-file boundary to keep the JSON-escaped length
/// within [`MAX_SESSION_DIFF_BYTES`], per file in original order.
///
/// The escaped length of a chunk is `serde_json::to_string(chunk).len() - 2`
/// (subtracting the wrapping quotes JSON adds around a string value); escaping
/// is per-character, so these lengths are additive across chunks. The first
/// chunk that would push the running total over the cap — even the very
/// first, if a single file's diff alone exceeds it — is dropped along with
/// every chunk after it, and `truncated` is set.
///
/// `pub(super)` for a focused, fast `session::tests` unit test over synthetic
/// chunk boundaries, independent of the (slower) real-git truncation test.
pub(super) fn cap_to_budget(text: &str) -> (String, bool) {
    let mut included = String::new();
    let mut used = 0usize;
    let mut truncated = false;
    for chunk in diff_git_chunks(text) {
        let len = escaped_len(chunk);
        if used.saturating_add(len) > MAX_SESSION_DIFF_BYTES {
            truncated = true;
            break;
        }
        included.push_str(chunk);
        used += len;
    }
    (included, truncated)
}

/// The JSON-escaped byte length `text` would occupy as a JSON string value.
fn escaped_len(text: &str) -> usize {
    // A `&str` always serializes to a JSON string; this cannot fail.
    serde_json::to_string(text)
        .expect("serializing a &str to JSON string cannot fail")
        .len()
        - 2
}

/// The canonical `runtime/session_no_worktree` error: the session has no
/// worktree to diff (an in-place session, or one launched with a bare `cwd`).
fn session_no_worktree(id: &SessionId) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "session_no_worktree",
        format!("session {} has no worktree to diff", id.0),
        Some(
            "diff a session started with --repo/--branch (or otherwise bound to a worktree); an in-place session has no dedicated worktree to diff against a base"
                .to_owned(),
        ),
    )
}

/// The canonical `runtime/session_diff_base_unresolved` error: `base` could not
/// be resolved to a merge-base against `HEAD` in the session's worktree (an
/// unknown ref, or one with no common ancestor).
fn session_diff_base_unresolved(base: &str, detail: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "session_diff_base_unresolved",
        format!("could not resolve base ref {base:?} in the session's worktree: {detail}"),
        Some(
            "verify the base ref exists in the worktree (fetch it first if it only lives on a remote)"
                .to_owned(),
        ),
    )
}

/// The canonical `runtime/session_diff_failed` error: a git subprocess used to
/// compute the diff failed for a reason other than an unresolved base ref.
fn session_diff_failed(detail: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "session_diff_failed",
        detail.to_owned(),
        None,
    )
}

/// The canonical `runtime/session_diff_store_error` error: the worktree binding
/// store could not be read while resolving the recorded base branch.
fn session_diff_store_error(err: &std::io::Error) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "session_diff_store_error",
        format!("failed to read the worktree binding store: {err}"),
        None,
    )
}
