//! The single-ref `git fetch` from `origin` behind every daemon fetch.
//!
//! Each fetch lands in a per-request ref under `refs/pohunek/fetch` instead of
//! `FETCH_HEAD`, which concurrent fetches in one repository would share, and
//! returns a [`FetchedBase`] guard that pins the commit until it is dropped.
//! [`FetchScope`] selects the owner-visibility profile: the daemon's own base
//! and branch fetches, or the fetch of an untrusted `fetch_ref`.

// Rust guideline compliant 2026-11-13

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use tracing::warn;

use super::{
    admin_lock, check_fetch_source, git_capture, git_command, git_run, output_failure_message,
    run_output_bounded, run_output_bounded_then, share_within, OutputPolicy, GIT_COMMAND_TIMEOUT,
};

/// Namespace for the short-lived per-request refs a base-branch fetch lands in.
///
/// `FETCH_HEAD` is a single file per repository, so two concurrent fetches in
/// one repository would overwrite each other's tip. Each fetch instead writes
/// its own ref under this namespace, resolves it to a commit id, and deletes it.
const FETCH_REF_NAMESPACE: &str = "refs/pohunek/fetch";

/// Random bytes in a fetch ref's nonce. 128 bits make a collision between two
/// requests (including daemons in separate PID namespaces sharing a repository)
/// negligible.
const FETCH_REF_NONCE_BYTES: usize = 16;

/// A base fetched from `origin` and pinned by a per-request ref.
///
/// The ref keeps the commit reachable (against `git gc`, concurrent maintenance
/// or a pre-create hook) until the guard is dropped; dropping deletes the ref,
/// but only while it still points at [`Self::ref_oid`], so a ref this request did
/// not create is never removed.
#[derive(Debug)]
pub(crate) struct FetchedBase {
    /// Peeled commit the worktree starts from.
    pub(super) commit: String,
    /// Raw object id the ref holds; differs from `commit` when the fetched ref
    /// is an annotated tag. This is the expected old value of the guarded delete.
    ref_oid: String,
    repository: PathBuf,
    ref_name: String,
}

impl Drop for FetchedBase {
    fn drop(&mut self) {
        if let Err(message) = git_run(
            &self.repository,
            &["update-ref", "-d", &self.ref_name, &self.ref_oid],
        ) {
            warn!(
                fetch_ref = %self.ref_name,
                error = %message,
                "could not delete the temporary fetch ref"
            );
        }
    }
}

/// Name of the per-request ref a fetch lands in: process id plus a random nonce.
pub(crate) fn fetch_ref_name() -> Result<String, String> {
    let mut nonce = [0_u8; FETCH_REF_NONCE_BYTES];
    getrandom::getrandom(&mut nonce)
        .map_err(|err| format!("could not generate a fetch ref nonce: {err}"))?;
    let nonce = nonce.iter().fold(String::new(), |mut hex, byte| {
        // Writing to a `String` cannot fail.
        let _ = write!(hex, "{byte:02x}");
        hex
    });
    Ok(format!(
        "{FETCH_REF_NAMESPACE}/{}-{nonce}",
        std::process::id()
    ))
}

/// Fetch a single ref from `origin` without tags into a fresh per-request ref and
/// return the guard that pins the fetched commit.
///
/// The tip lands in a per-request ref ([`fetch_ref_name`]) rather than
/// `FETCH_HEAD`, which concurrent fetches in one repository would share.
pub(super) fn fetch_origin(repo: &Path, base_branch: &str) -> Result<FetchedBase, String> {
    fetch_into_ref(repo, base_branch, &fetch_ref_name()?)
}

/// Which owner-visibility profile a single-ref `git fetch` from `origin` runs with.
///
/// The daemon has two fetch profiles. Its own base and remote-branch fetches
/// run as [`FetchScope::Tracked`]: git defaults, so opportunistic
/// remote-tracking updates move `refs/remotes/origin/*` like any owner's
/// fetch (retention's `unpushed_hold` reads those refs). The fetch of an
/// untrusted `fetch_ref` runs as [`FetchScope::Untrusted`]: it writes no
/// `FETCH_HEAD` entry, recurses into no submodule and follows no configured
/// `remote.origin.fetch` refspec, so it moves no ref outside the
/// session-owned destination it creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchScope {
    /// The daemon's own fetch: `FETCH_HEAD` and submodule recursion as git
    /// defaults, opportunistic remote-tracking updates enabled.
    Tracked,
    /// The fetch of an untrusted ref: `--no-write-fetch-head`,
    /// `--no-recurse-submodules` and `--refmap=`.
    Untrusted,
}

impl FetchScope {
    /// Extra `git fetch` flags this scope adds after `--no-tags`.
    fn extra_args(self) -> &'static [&'static str] {
        match self {
            Self::Tracked => &[],
            // `--no-write-fetch-head` needs git 2.29 or newer; older gits
            // fail the fetch instead of writing. `--refmap=` exists since
            // 2.1, so it adds no new minimum.
            Self::Untrusted => &[
                "--no-write-fetch-head",
                "--no-recurse-submodules",
                "--refmap=",
            ],
        }
    }
}

/// The `git fetch` of `source` from `origin` into `fetch_ref`.
///
/// Automatic maintenance is disabled: its `git gc` runs `worktree prune`, which
/// can overlap a locked add and inspect its incomplete admin entry.
/// `scope` adds its flags after `--no-tags` (see [`FetchScope`]), and
/// `--end-of-options` keeps the refspec positional.
pub(super) fn fetch_command(
    repo: &Path,
    source: &str,
    fetch_ref: &str,
    scope: FetchScope,
) -> Result<Command, String> {
    let mut cmd = git_command(repo)?;
    cmd.args(["-c", "gc.auto=0", "-c", "maintenance.auto=false"])
        .arg("fetch")
        .arg("--no-tags");
    for extra in scope.extra_args() {
        cmd.arg(extra);
    }
    cmd.arg("origin")
        .arg("--end-of-options")
        .arg(format!("{source}:{fetch_ref}"));
    Ok(cmd)
}

/// The daemon's own fetch of `source` into `fetch_ref`: [`fetch_into_ref_with`]
/// with [`FetchScope::Tracked`].
pub(super) fn fetch_into_ref(
    repo: &Path,
    source: &str,
    fetch_ref: &str,
) -> Result<FetchedBase, String> {
    fetch_into_ref_with(repo, source, fetch_ref, FetchScope::Tracked)
}

/// Fetch `source` from `origin` into `fetch_ref` and return the guard that
/// pins the fetched commit.
///
/// `scope` selects the owner-visibility profile of [`fetch_command`]:
/// [`FetchScope::Tracked`] for the daemon's own fetches,
/// [`FetchScope::Untrusted`] for an untrusted `fetch_ref`. The refspec has
/// no `+` and the destination must not exist beforehand, so a ref this
/// request did not create is never overwritten. A failed fetch deletes only
/// the destination it was proven to create (see below); nothing else is
/// removed.
///
/// `--end-of-options` keeps the refspec positional even if a caller forgets
/// the leading-dash guard (defense in depth against argv flag injection).
pub(crate) fn fetch_into_ref_with(
    repo: &Path,
    source: &str,
    fetch_ref: &str,
    scope: FetchScope,
) -> Result<FetchedBase, String> {
    check_fetch_source(repo, source)?;
    let mut probe = git_command(repo)?;
    probe.args(["rev-parse", "--verify", "--quiet", fetch_ref]);
    let probed = run_output_bounded(probe, GIT_COMMAND_TIMEOUT)?;
    match probed.status.code() {
        Some(1) => {}
        Some(0) => return Err(format!("fetch ref {fetch_ref} already exists")),
        _ => return Err(output_failure_message(&probed)),
    }
    let cmd = fetch_command(repo, source, fetch_ref, scope)?;
    // The fetch's connectivity check reads every `worktrees/<name>/HEAD`, and
    // `git worktree add` first writes a null-oid placeholder there, which fails
    // the check with `bad object worktrees/<name>/HEAD`. Holding the admin lock
    // shared keeps the fetch from overlapping an add or prune on the same
    // repository while fetches still run side by side.
    let lock = admin_lock(repo)?;
    let guard = share_within(&lock, fetch_lock_wait())?;
    // Fetch diagnostics are not parsed: a successful fetch already owns its ref.
    let output = run_output_bounded_then(
        cmd,
        fetch_command_timeout(),
        OutputPolicy::StatusOnly,
        move || drop(guard),
    )?;
    if !output.status.success() {
        // The probe above proved the destination was absent, so a failed
        // fetch deletes only the ref the fetch itself created: nothing else
        // can name a session-owned ref here.
        if let Err(delete) = git_run(repo, &["update-ref", "-d", fetch_ref]) {
            warn!(
                fetch_ref,
                error = %delete,
                "could not delete the temporary fetch ref after a failed fetch"
            );
        }
        return Err(output_failure_message(&output));
    }
    // The fetch created the ref (it was absent above), so this request owns it
    // even when it does not resolve to a commit.
    let resolved = git_capture(repo, &["rev-parse", "--verify", fetch_ref]).and_then(|ref_oid| {
        git_capture(
            repo,
            &["rev-parse", "--verify", &format!("{fetch_ref}^{{commit}}")],
        )
        .map(|commit| (ref_oid, commit))
    });
    let (ref_oid, commit) = match resolved {
        Ok(ids) => ids,
        Err(message) => {
            if let Err(delete) = git_run(repo, &["update-ref", "-d", fetch_ref]) {
                warn!(fetch_ref, error = %delete, "could not delete the temporary fetch ref");
            }
            return Err(message);
        }
    };
    Ok(FetchedBase {
        commit,
        ref_oid,
        repository: repo.to_path_buf(),
        ref_name: fetch_ref.to_owned(),
    })
}

/// How long the `git fetch` process may run: the git command bound, or the
/// bound a test injected for its thread.
fn fetch_command_timeout() -> Duration {
    #[cfg(test)]
    if let Some(timeout) = FETCH_COMMAND_TIMEOUT_OVERRIDE.with(std::cell::Cell::get) {
        return timeout;
    }
    GIT_COMMAND_TIMEOUT
}

#[cfg(test)]
thread_local! {
    /// Test-only bound on this thread's `git fetch` process.
    pub(super) static FETCH_COMMAND_TIMEOUT_OVERRIDE: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

/// How long a fetch waits for the admin lock: the git command bound, or the
/// bound a test injected for its thread.
fn fetch_lock_wait() -> Duration {
    #[cfg(test)]
    if let Some(wait) = FETCH_LOCK_WAIT_OVERRIDE.with(std::cell::Cell::get) {
        return wait;
    }
    GIT_COMMAND_TIMEOUT
}

#[cfg(test)]
thread_local! {
    /// Test-only bound on this thread's fetch wait for the admin lock.
    pub(super) static FETCH_LOCK_WAIT_OVERRIDE: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}
