//! One extra ref a `session.new` fetches without checking it out.
//!
//! [`fetch_session_temp`] fetches `source_ref` from `origin` into a private
//! per-request ref and checks it peels to the expected commit. The caller
//! promotes the commit to the daemon-chosen session ref with
//! [`promote_session_fetched_ref`] once the checkout stands and before the
//! post-create hook runs.
//! The session ref lives as long as the worktree binding; binding cleanup
//! deletes it with [`delete_session_fetched_ref`]. A reused worktree reports
//! the ref without refetching through [`require_session_fetched_ref`], which
//! requires the expected commit instead of trusting whatever the ref holds.

// Rust guideline compliant 2026-11-12

use std::path::Path;

use protocol::{ErrorClass, FetchRefRequest, FetchedRef, ProtocolError};
use tracing::warn;

use super::{
    error, fetch_into_ref_with, fetch_ref_name, git_command, git_run, has_origin,
    output_failure_message, run_output_bounded, worktree_add_error, FetchScope, FetchedBase,
    GIT_COMMAND_TIMEOUT,
};

/// Namespace of the per-session refs a `fetch_ref` fetch pins.
///
/// The caller names the source ref and the expected commit; the daemon chooses
/// the destination, `refs/pohunek/sessions/<session_id>/fetched`, so a request
/// can neither overwrite another session's ref nor address a ref outside this
/// namespace. The ref lives as long as the worktree binding: binding cleanup
/// deletes it.
const SESSION_FETCHED_REF_NAMESPACE: &str = "refs/pohunek/sessions";

/// The daemon-chosen destination of a `fetch_ref` fetch for `session_id`.
pub(crate) fn session_fetched_ref_name(session_id: &str) -> String {
    format!("{SESSION_FETCHED_REF_NAMESPACE}/{session_id}/fetched")
}

/// Whether `commit` is a full lowercase hex object id, 40 (SHA-1) or 64
/// (SHA-256) digits: the only shape a `fetch_ref` commit takes.
fn is_full_commit_id(commit: &str) -> bool {
    matches!(commit.len(), 40 | 64)
        && commit
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Reject a `fetch_ref` request whose shape can never fetch: a source ref that
/// is not a full `refs/` name git accepts, or a commit that is not a full
/// lowercase hex id.
///
/// Runs at the trust boundary (a socket string becomes a `git` argument), so
/// the refspec stays positional and no flag injection reaches `git`. The
/// `check-ref-format` half (no `--allow-onelevel`) runs in
/// [`check_session_fetch_ref_name`], where a repository is at hand, because a
/// one-level name is never a full ref while [`super::check_fetch_source`]
/// accepts one.
pub(crate) fn check_session_fetch_request(
    source_ref: &str,
    commit: &str,
) -> Result<(), ProtocolError> {
    if !source_ref.starts_with("refs/") {
        return Err(ProtocolError::bad_request(format!(
            "session.new fetch_ref source_ref must start with 'refs/': {source_ref:?}"
        )));
    }
    if !is_full_commit_id(commit) {
        return Err(ProtocolError::bad_request(format!(
            "session.new fetch_ref commit must be a full lowercase hex commit id (40 or 64 digits): {commit:?}"
        )));
    }
    Ok(())
}

/// The `git check-ref-format` half of [`check_session_fetch_request`]: runs
/// where a repository is at hand, before any fetch. The default rules (no
/// `--allow-onelevel`, unlike [`super::check_fetch_source`]) require a full
/// ref, so a one-level name is rejected here.
fn check_session_fetch_ref_name(repo: &Path, source_ref: &str) -> Result<(), ProtocolError> {
    let mut cmd = match git_command(repo) {
        Ok(cmd) => cmd,
        Err(message) => return Err(fetch_ref_failed(source_ref, &message)),
    };
    // No `--end-of-options`: `check-ref-format` takes a single pattern and
    // rejects that flag. Argv injection is already excluded: a source ref
    // that does not start with `refs/` never reaches `git`.
    cmd.args(["check-ref-format", source_ref]);
    match run_output_bounded(cmd, GIT_COMMAND_TIMEOUT) {
        Ok(output) if output.status.success() => Ok(()),
        Ok(_) => Err(ProtocolError::bad_request(format!(
            "session.new fetch_ref source_ref is not a valid git ref name: {source_ref:?}"
        ))),
        Err(message) => Err(fetch_ref_failed(source_ref, &message)),
    }
}

/// Fetch `fetch.source_ref` from `origin` into a private per-request ref and
/// check it peels to `fetch.commit`.
///
/// The returned guard pins the commit (against `git gc`, concurrent
/// maintenance or a hook) until the caller promotes it or drops it; dropping
/// deletes the temp ref. Runs after base resolution and before any hook, so a
/// refused request leaves no session, binding, worktree, temp ref or session
/// ref behind. The fetch writes no `FETCH_HEAD` entry, recurses into no
/// submodule, and follows no configured `remote.origin.fetch` refspec, so it
/// moves no ref outside the session-owned temp ref it created.
///
/// # Errors
///
/// `bad_request` for a malformed request, `fetch_ref_not_found` when there is
/// no `origin` or it has no such ref, `fetch_ref_commit_mismatch` when the
/// fetched ref peels to another commit, `fetch_ref_failed` otherwise.
pub(crate) fn fetch_session_temp(
    repo: &Path,
    fetch: &FetchRefRequest,
) -> Result<FetchedBase, ProtocolError> {
    check_session_fetch_request(&fetch.source_ref, &fetch.commit)?;
    check_session_fetch_ref_name(repo, &fetch.source_ref)?;
    if !has_origin(repo) {
        return Err(fetch_ref_not_found(
            &fetch.source_ref,
            "the repository has no origin remote",
        ));
    }
    let temp = fetch_ref_name().map_err(|message| fetch_ref_failed(&fetch.source_ref, &message))?;
    let fetched = match fetch_into_ref_with(repo, &fetch.source_ref, &temp, FetchScope::Untrusted) {
        Ok(fetched) => fetched,
        Err(message) if message.contains("timed out") => {
            return Err(fetch_ref_failed(&fetch.source_ref, &message));
        }
        Err(message) => match remote_has_session_source(repo, &fetch.source_ref) {
            Ok(false) => {
                return Err(fetch_ref_not_found(
                    &fetch.source_ref,
                    "origin has no such ref",
                ));
            }
            Ok(true) | Err(_) => return Err(fetch_ref_failed(&fetch.source_ref, &message)),
        },
    };
    if fetched.commit != fetch.commit {
        // The temp ref dies with the guard; nothing is promoted.
        return Err(fetch_ref_commit_mismatch(
            &fetch.source_ref,
            &fetch.commit,
            &fetched.commit,
        ));
    }
    Ok(fetched)
}

/// Promote `temp` to the session ref of `session_id` and report it.
///
/// Runs once the checkout stands and before the post-create hook runs, so a
/// pre-create hook (which fires before the fetch) never observes a session
/// ref for a binding that does not exist yet. The write
/// is create-only: a ref that already holds the expected commit is success, a
/// ref that holds another commit is `worktree_add_failed`. The temp ref dies
/// with its guard.
///
/// Builds the whole [`FetchedRef`] from the request: no caller patches a
/// half-built value.
pub(crate) fn promote_session_fetched_ref(
    repo: &Path,
    session_id: &str,
    branch: &str,
    fetch: &FetchRefRequest,
    temp: FetchedBase,
) -> Result<FetchedRef, ProtocolError> {
    let ref_name = session_fetched_ref_name(session_id);
    match read_session_ref_commit(repo, &ref_name) {
        Ok(Some(existing)) if existing == temp.commit => {}
        Ok(Some(existing)) => {
            return Err(worktree_add_error(
                branch,
                &format!("session fetch ref {ref_name} already holds {existing}"),
            ));
        }
        Ok(None) => {
            if let Err(message) = git_run(repo, &["update-ref", &ref_name, &temp.commit, ""]) {
                return Err(worktree_add_error(branch, &message));
            }
        }
        Err(message) => return Err(fetch_ref_failed(&fetch.source_ref, &message)),
    }
    // The temp ref dies here; the session ref below is owned by the binding
    // until binding cleanup deletes it.
    drop(temp);
    Ok(FetchedRef {
        fetched_ref: ref_name,
        source_ref: fetch.source_ref.clone(),
        commit: fetch.commit.clone(),
    })
}

/// Report the session ref of `session_id` for a reused worktree without
/// refetching, requiring the expected commit.
///
/// A ref that holds another commit, or no ref at all, is `fetch_ref_failed`:
/// the reuse path never trusts whatever the ref holds.
pub(crate) fn require_session_fetched_ref(
    repo: &Path,
    session_id: &str,
    fetch: &FetchRefRequest,
) -> Result<FetchedRef, ProtocolError> {
    let ref_name = session_fetched_ref_name(session_id);
    match read_session_ref_commit(repo, &ref_name) {
        Ok(Some(commit)) if commit == fetch.commit => Ok(FetchedRef {
            fetched_ref: ref_name,
            source_ref: fetch.source_ref.clone(),
            commit,
        }),
        Ok(Some(commit)) => Err(fetch_ref_failed(
            &fetch.source_ref,
            &format!(
                "the session ref {ref_name} holds {commit} but the request expected {}",
                fetch.commit
            ),
        )),
        Ok(None) => Err(fetch_ref_failed(
            &fetch.source_ref,
            &format!("the session ref {ref_name} is missing for a reused worktree"),
        )),
        Err(message) => Err(fetch_ref_failed(&fetch.source_ref, &message)),
    }
}

/// Whether `origin` advertises exactly `source_ref`.
///
/// A failed fetch is `fetch_ref_not_found` only when `git ls-remote` proves
/// the ref absent; any other failure, including an unreachable remote, is
/// `fetch_ref_failed`.
fn remote_has_session_source(repo: &Path, source_ref: &str) -> Result<bool, String> {
    let mut cmd = git_command(repo)?;
    cmd.args([
        "ls-remote",
        "--exit-code",
        "origin",
        "--end-of-options",
        source_ref,
    ]);
    let output = run_output_bounded(cmd, GIT_COMMAND_TIMEOUT)?;
    match output.status.code() {
        Some(0) => Ok(String::from_utf8_lossy(&output.stdout).lines().any(|line| {
            line.split_once('\t')
                .is_some_and(|(_, name)| name == source_ref)
        })),
        // `--exit-code` reports "no matching refs" as 2.
        Some(2) => Ok(false),
        _ => Err(output_failure_message(&output)),
    }
}

/// Read the commit the session ref `ref_name` holds, if any.
///
/// An absent ref is `Ok(None)`; an unreadable repository is an error like any
/// other git failure of the bind.
fn read_session_ref_commit(repo: &Path, ref_name: &str) -> Result<Option<String>, String> {
    let mut cmd = git_command(repo)?;
    cmd.args(["rev-parse", "--verify", "--quiet", ref_name]);
    match run_output_bounded(cmd, GIT_COMMAND_TIMEOUT) {
        Ok(output) if output.status.success() => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        )),
        Ok(_) => Ok(None),
        Err(message) => Err(message),
    }
}

/// Delete the session ref of `session_id`, best-effort: binding cleanup owns
/// the ref once the bind settled, so every path that drops a binding drops
/// the ref with it. A missing repository (or ref) is not an error.
pub(crate) fn delete_session_fetched_ref(repo: &Path, session_id: &str) {
    if !repo.is_dir() {
        return;
    }
    let ref_name = session_fetched_ref_name(session_id);
    if let Err(message) = git_run(repo, &["update-ref", "-d", &ref_name]) {
        warn!(
            fetch_ref = %ref_name,
            error = %message,
            "could not delete the session fetch ref during binding cleanup"
        );
    }
}

fn fetch_ref_not_found(source_ref: &str, why: &str) -> ProtocolError {
    error(
        ErrorClass::Runtime,
        "fetch_ref_not_found",
        format!("cannot fetch {source_ref:?} from origin: {why}"),
        Some("push the ref first, or start without the extra fetch".to_owned()),
    )
}

fn fetch_ref_failed(source_ref: &str, message: &str) -> ProtocolError {
    error(
        ErrorClass::Runtime,
        "fetch_ref_failed",
        format!("fetching {source_ref:?} from origin failed: {message}"),
        Some("check the remote, or start without the extra fetch".to_owned()),
    )
}

fn fetch_ref_commit_mismatch(source_ref: &str, expected: &str, actual: &str) -> ProtocolError {
    error(
        ErrorClass::Runtime,
        "fetch_ref_commit_mismatch",
        format!(
            "fetching {source_ref:?} from origin found commit {actual} but the request expected {expected}"
        ),
        Some("fetch the ref and pass the commit it points to".to_owned()),
    )
}
