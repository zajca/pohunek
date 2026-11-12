//! `session.new` with `fetch_ref`, through the public control socket against
//! real git repositories and a bare `origin`.
//!
//! The scenarios pin that the extra ref fetch lands the expected commit under
//! the session ref without checking it out, that a refused request leaves no
//! session, binding, worktree, temp ref or session ref behind, that the fetch
//! composes with `branch_source: remote` (it draws its own ref, not the
//! branch's), and that `FETCH_HEAD` is untouched by the fetch.

use super::*;

use pohunek_service_config::RequiredHook;
use protocol::{BranchSource, FetchRefRequest, FetchedRef, SessionNewResult};

/// The source ref every scenario fetches.
const SOURCE_REF: &str = "refs/pull/42/head";

/// A started daemon, a working repository and the bare `origin` it is cloned
/// from.
struct Rig {
    control: Framed<UnixStream, LinesCodec>,
    shutdown: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
    repo: TestEnv,
    origin: TestDir,
    worktree_root: TestDir,
    _socket: TestSocket,
    _state: TestDir,
}

impl Rig {
    async fn start(tag: &str) -> Self {
        Self::start_with_hooks(tag, None, None, Vec::new()).await
    }

    /// `pre_create` is the body of a host-global `pre-create` hook (used as a
    /// marker that a refused request never ran a hook), `post_create` the
    /// body of a host-global `post-create` hook; `required` lists the hooks
    /// `[hooks] required` names (row 9 needs a required post-create).
    async fn start_with_hooks(
        tag: &str,
        pre_create: Option<&str>,
        post_create: Option<&str>,
        required: Vec<RequiredHook>,
    ) -> Self {
        let repo = init_git_repo();
        let origin = temp_dir(&format!("{tag}-origin"));
        let worktree_root = temp_dir(&format!("{tag}-wt"));
        let state = temp_dir(&format!("{tag}-state"));
        let socket = temp_socket(tag);
        let config_dir = match (pre_create, post_create) {
            (None, None) => None,
            (pre, post) => {
                let config_dir = socket.dir.join("config");
                std::fs::create_dir_all(config_dir.join("hooks"))
                    .expect("create host hook directory");
                if let Some(body) = pre {
                    write_executable(&config_dir.join("hooks/pre-create"), body);
                }
                if let Some(body) = post {
                    write_executable(&config_dir.join("hooks/post-create"), body);
                }
                Some(config_dir)
            }
        };
        let config = SessionRegistryConfig {
            shell_command: support::hermetic_shell(),
            worktree_root: Some(worktree_root.to_path_buf()),
            store_path: Some(state.join("metadata.jsonl")),
            config_dir,
            hook_required: required,
            ..SessionRegistryConfig::default()
        };
        let (shutdown, server, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
        let control = connect(&socket).await;
        let rig = Self {
            control,
            shutdown,
            server,
            repo,
            origin,
            worktree_root,
            _socket: socket,
            _state: state,
        };
        let origin_path = rig.origin.join("origin.git");
        rig.git_in(
            rig.repo.cwd(),
            &[
                "clone",
                "-q",
                "--bare",
                ".",
                &origin_path.display().to_string(),
            ],
        );
        rig.git(&[
            "remote",
            "add",
            "origin",
            &origin_path.display().to_string(),
        ]);
        rig
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = self.server.await;
    }

    /// Runs git in `dir` with the scrubbed environment and returns trimmed stdout.
    fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        let out = self
            .repo
            .git_command()
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn git(&self, args: &[&str]) -> String {
        self.git_in(self.repo.cwd(), args)
    }

    /// Raw `git` in `dir`: no success assertion, for probing refs the fetch
    /// must not have moved.
    fn git_raw(&self, dir: &Path, args: &[&str]) -> std::process::Output {
        self.repo
            .git_command()
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("run git")
    }

    /// The commit `name` resolves to in the working repository, if any.
    fn rev(&self, name: &str) -> Option<String> {
        let out = self
            .repo
            .git_command()
            .arg("-C")
            .arg(self.repo.cwd())
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{name}^{{commit}}"),
            ])
            .output()
            .expect("run git rev-parse");
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// Commits a new file on `origin`'s default branch from a fresh clone and
    /// publishes it as the source ref of these scenarios.
    fn origin_source_ref(&self) -> String {
        let clone = temp_dir("fetch-ref-origin-clone");
        let dir = clone.join("clone");
        let origin = self.origin.join("origin.git").display().to_string();
        self.git_in(
            self.repo.cwd(),
            &["clone", "-q", &origin, &dir.display().to_string()],
        );
        self.git_in(&dir, &["config", "user.email", "test@example.com"]);
        self.git_in(&dir, &["config", "user.name", "Test"]);
        self.git_in(&dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("pr.txt"), "pr\n").expect("write file");
        self.git_in(&dir, &["add", "."]);
        self.git_in(&dir, &["commit", "-q", "-m", "pr"]);
        let head = self.git_in(&dir, &["rev-parse", "HEAD"]);
        self.git_in(
            &dir,
            &["push", "-q", "origin", &format!("HEAD:{SOURCE_REF}")],
        );
        head
    }

    /// Publishes `commit` on `origin` under `source_ref` and returns it.
    fn origin_publish_ref(&self, source_ref: &str, file: &str) -> String {
        let clone = temp_dir("fetch-ref-origin-extra");
        let dir = clone.join("clone-extra");
        let origin = self.origin.join("origin.git").display().to_string();
        self.git_in(
            self.repo.cwd(),
            &["clone", "-q", &origin, &dir.display().to_string()],
        );
        self.git_in(&dir, &["config", "user.email", "test@example.com"]);
        self.git_in(&dir, &["config", "user.name", "Test"]);
        self.git_in(&dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join(file), format!("{file}\n")).expect("write file");
        self.git_in(&dir, &["add", "."]);
        self.git_in(&dir, &["commit", "-q", "-m", file]);
        let head = self.git_in(&dir, &["rev-parse", "HEAD"]);
        self.git_in(
            &dir,
            &["push", "-q", "origin", &format!("HEAD:{source_ref}")],
        );
        head
    }

    /// Installs an uncommitted in-repo `pre-create` hook: the script file
    /// sits on disk in the repository checkout without any commit, so the
    /// base head stays put (the same pattern the `branch_source` CAS
    /// scenario uses). The hook scenarios below assert what the daemon
    /// reports: the daemon runs `pre-create` in the repository checkout,
    /// before it promotes the session ref.
    ///
    /// The hook records the commit the session ref holds at hook time
    /// through `seen` (empty when the session ref is not promoted yet).
    /// The caller owns `seen` through a live [`temp_dir`] guard; the helper
    /// creates the hooks directory, which outlives the scenario with the
    /// repository, the same way `branch_source`'s CAS hook installer does.
    fn in_repo_pre_create_hook(&self, seen: &Path) {
        let path = self.repo.cwd().join(".pohunek/hooks/pre-create");
        std::fs::create_dir_all(path.parent().expect("hook parent"))
            .expect("create the hooks directory");
        let body = format!(
            "#!/bin/sh\nGIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_COUNT=0 GIT_TERMINAL_PROMPT=0 git -C \"$POHUNEK_REPO\" rev-parse --verify --quiet \"refs/pohunek/sessions/$POHUNEK_SESSION_ID/fetched^{{commit}}\" > {} 2>/dev/null || : > {}\nexit 0\n",
            seen.display(),
            seen.display()
        );
        std::fs::write(&path, body).expect("write the hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("make the hook executable");
        }
    }

    fn params(&self, source_ref: &str, commit: &str) -> SessionNewParams {
        let mut params = session_params_for_worktree(
            &RuntimeRef::shell(),
            self.repo.cwd().to_path_buf(),
            "feat-fetch",
        );
        params.fetch_ref = Some(FetchRefRequest {
            source_ref: source_ref.to_owned(),
            commit: commit.to_owned(),
        });
        params
    }

    /// `branch_name` names the worktree branch so two creates on one repo
    /// do not collide on the default `feat-fetch` branch.
    fn params_on_branch(
        &self,
        source_ref: &str,
        commit: &str,
        branch_name: &str,
    ) -> SessionNewParams {
        let mut params = session_params_for_worktree(
            &RuntimeRef::shell(),
            self.repo.cwd().to_path_buf(),
            branch_name,
        );
        params.fetch_ref = Some(FetchRefRequest {
            source_ref: source_ref.to_owned(),
            commit: commit.to_owned(),
        });
        params
    }

    async fn new_session(&mut self, params: SessionNewParams) -> Response {
        create_session_with_params(&mut self.control, params).await
    }

    async fn sessions(&mut self) -> Vec<SessionInfo> {
        let list = Request::make(
            "fetch-ref-list",
            method::SESSION_LIST,
            serde_json::to_value(SessionListParams::default()).expect("serialize list params"),
        );
        serde_json::from_value(ok_payload(exchange(&mut self.control, &list).await))
            .expect("session list")
    }

    /// Asserts that a refused request left no session, worktree, session ref
    /// or private fetch ref, and that the worktree root is empty.
    async fn assert_nothing_left(&mut self) {
        assert!(
            self.sessions().await.is_empty(),
            "no session record is left"
        );
        let worktrees = self.git(&["worktree", "list", "--porcelain"]);
        assert_eq!(
            worktrees
                .lines()
                .filter(|line| line.starts_with("worktree "))
                .count(),
            1,
            "only the main checkout is registered: {worktrees}"
        );
        assert_eq!(
            self.git(&["for-each-ref", "refs/pohunek/sessions"]),
            "",
            "no session ref is left"
        );
        assert_eq!(
            self.git(&["for-each-ref", "refs/pohunek/fetch"]),
            "",
            "the private fetch ref is deleted"
        );
        let leftover: Vec<_> = std::fs::read_dir(&*self.worktree_root)
            .expect("read the worktree root")
            .collect();
        assert!(
            leftover.is_empty(),
            "no worktree directory is left: {leftover:?}"
        );
    }
}

fn fetch_head_state(rig: &Rig) -> Option<(String, std::time::SystemTime)> {
    let path = rig.repo.cwd().join(".git/FETCH_HEAD");
    let content = std::fs::read_to_string(&path).ok()?;
    let mtime = std::fs::metadata(&path)
        .expect("stat FETCH_HEAD")
        .modified()
        .expect("FETCH_HEAD mtime");
    Some((content, mtime))
}

fn started(response: Response) -> (SessionInfo, FetchedRef) {
    let result: SessionNewResult =
        serde_json::from_value(ok_payload(response)).expect("session.new result");
    let fetched = result.fetched_ref.expect("fetched_ref is reported");
    (result.session, fetched)
}

/// A pull-request head is fetched without checkout: the session ref holds the
/// expected commit, `FETCH_HEAD` is untouched, and the worktree starts on the
/// requested branch.
#[tokio::test]
async fn a_pull_request_head_is_fetched_without_checkout() {
    let mut rig = Rig::start("fetch-ref-ok").await;
    let base = rig.git(&["rev-parse", "HEAD"]);
    let expected = rig.origin_source_ref();
    assert_ne!(expected, base, "the fetched head differs from the base tip");
    // Touch FETCH_HEAD through a plain fetch so the scenario can prove the
    // `fetch_ref` fetch below leaves it alone.
    rig.git(&["fetch", "-q", "origin"]);
    let before = fetch_head_state(&rig).expect("FETCH_HEAD exists");

    let (session, fetched) = started(rig.new_session(rig.params(SOURCE_REF, &expected)).await);

    assert_eq!(fetched.source_ref, SOURCE_REF);
    assert_eq!(fetched.commit, expected);
    let session_id = session.id.0.clone();
    assert_eq!(
        fetched.fetched_ref,
        format!("refs/pohunek/sessions/{session_id}/fetched")
    );
    assert_eq!(
        rig.rev(&fetched.fetched_ref),
        Some(expected.clone()),
        "the session ref holds the expected commit"
    );
    let worktree = session.worktree_path.as_deref().expect("worktree path");
    assert_eq!(
        rig.git_in(worktree, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "feat-fetch",
        "the worktree checks out the requested branch, not the fetched ref"
    );
    // Row 1 pins that the checkout stands on the base head, not the
    // fetched commit: the commit exists as an object in the worktree (the
    // case table's `cat-file -e` probe) but the new branch does not contain
    // it.
    assert_eq!(
        rig.git_in(worktree, &["rev-parse", "HEAD"]),
        base,
        "the checkout starts on the base tip"
    );
    assert!(
        rig.repo
            .git_command()
            .arg("-C")
            .arg(worktree)
            .args(["cat-file", "-e", &format!("{expected}^{{commit}}")])
            .output()
            .expect("run git cat-file")
            .status
            .success(),
        "the fetched commit exists in the worktree"
    );
    assert!(
        !rig.repo
            .git_command()
            .arg("-C")
            .arg(rig.repo.cwd())
            .args(["merge-base", "--is-ancestor", &expected, &base])
            .output()
            .expect("run git merge-base")
            .status
            .success(),
        "the new branch does not contain the fetched commit"
    );
    // The `fetch_ref` fetch itself writes no `FETCH_HEAD` entry: the file
    // holds only the base fetch's record (pre-existing daemon behavior for
    // every worktree create), never the source ref or a private ref.
    let after = fetch_head_state(&rig).expect("FETCH_HEAD exists");
    assert_eq!(
        after.0.lines().count(),
        before.0.lines().count(),
        "the fetch_ref fetch adds no FETCH_HEAD entry: {after:?}"
    );
    assert!(
        !after.0.contains(SOURCE_REF) && !after.0.contains("refs/pohunek/"),
        "FETCH_HEAD names neither the source ref nor a private ref: {after:?}"
    );
    assert!(
        rig.sessions()
            .await
            .iter()
            .any(|info| info.id == session.id),
        "the session is listed"
    );

    // `session.remove` drops the session ref with the binding.
    let remove = Request::make(
        "fetch-ref-remove",
        method::SESSION_REMOVE,
        serde_json::to_value(session.id.0.clone()).expect("serialize session id"),
    );
    let _: SessionRemoveResult =
        serde_json::from_value(ok_payload(exchange(&mut rig.control, &remove).await))
            .expect("session.remove result");
    assert_eq!(
        rig.rev(&fetched.fetched_ref),
        None,
        "session.remove deletes the session ref"
    );
    rig.stop().await;
}

/// Row 2 of the case table: a source ref `origin` does not have fails with
/// `fetch_ref_not_found` and leaves nothing behind, with no hook side effect:
/// the host `pre-create` hook marker stays absent because the fetch runs
/// before any hook.
#[tokio::test]
async fn an_unknown_source_ref_fails_without_leaving_anything() {
    let marker_dir = temp_dir("fetch-ref-absent-marker");
    let marker = marker_dir.join("pre-create-ran");
    let mut rig = Rig::start_with_hooks(
        "fetch-ref-absent",
        Some(&format!("#!/bin/sh\ntouch {}\n", marker.display())),
        None,
        Vec::new(),
    )
    .await;
    let main = rig.git(&["rev-parse", "HEAD"]);

    let error = err_payload(
        rig.new_session(rig.params("refs/pull/404/head", &main))
            .await,
    );

    assert_eq!(error.code, "fetch_ref_not_found", "{}", error.msg);
    assert!(
        !marker.exists(),
        "the host pre-create hook never ran for a refused fetch"
    );
    rig.assert_nothing_left().await;
    // Positive control: the same host hook on a successful create leaves
    // its marker, so the absent marker above proves the hook never ran —
    // it is not an artifact of a missing marker directory.
    let expected = rig.origin_source_ref();
    started(rig.new_session(rig.params(SOURCE_REF, &expected)).await);
    assert!(
        marker.exists(),
        "the host pre-create hook runs on a successful create"
    );
    rig.stop().await;
}

/// Row 3 of the case table: a repository without an `origin` remote fails
/// with `fetch_ref_not_found` (naming the missing remote, mirroring
/// `remote_branch_not_found`) and leaves nothing behind, with no hook side
/// effect.
#[tokio::test]
async fn a_repository_without_origin_fails_without_leaving_anything() {
    let marker_dir = temp_dir("fetch-ref-no-origin-marker");
    let marker = marker_dir.join("pre-create-ran");
    let mut rig = Rig::start_with_hooks(
        "fetch-ref-no-origin",
        Some(&format!("#!/bin/sh\ntouch {}\n", marker.display())),
        None,
        Vec::new(),
    )
    .await;
    rig.git(&["remote", "remove", "origin"]);
    let main = rig.git(&["rev-parse", "HEAD"]);

    let error = err_payload(rig.new_session(rig.params(SOURCE_REF, &main)).await);

    assert_eq!(error.code, "fetch_ref_not_found", "{}", error.msg);
    assert!(
        error.msg.contains("no origin remote"),
        "the error names the missing remote: {}",
        error.msg
    );
    assert!(
        !marker.exists(),
        "the host pre-create hook never ran for a refused fetch"
    );
    rig.assert_nothing_left().await;
    rig.stop().await;
}

/// Row 4 of the case table: a fetch that peels to another commit fails with `fetch_ref_commit_mismatch`
/// naming both ids and leaves nothing behind.
#[tokio::test]
async fn a_commit_mismatch_fails_without_leaving_anything() {
    let marker_dir = temp_dir("fetch-ref-mismatch-marker");
    let marker = marker_dir.join("pre-create-ran");
    let mut rig = Rig::start_with_hooks(
        "fetch-ref-mismatch",
        Some(&format!("#!/bin/sh\ntouch {}\n", marker.display())),
        None,
        Vec::new(),
    )
    .await;
    let expected = rig.origin_source_ref();
    let other = rig.git(&["rev-parse", "HEAD"]);
    let error = err_payload(rig.new_session(rig.params(SOURCE_REF, &other)).await);

    assert_eq!(error.code, "fetch_ref_commit_mismatch", "{}", error.msg);
    assert!(
        error.msg.contains(&other) && error.msg.contains(&expected),
        "the error names both the expected and the fetched commit: {}",
        error.msg
    );
    assert!(
        !marker.exists(),
        "the host pre-create hook never ran for a refused fetch"
    );
    rig.assert_nothing_left().await;
    rig.stop().await;
}

/// Row 6 of the case table: without `branch` there is no worktree to pin
/// the ref for: `bad_request`.
#[tokio::test]
async fn fetch_ref_without_branch_is_rejected() {
    let mut rig = Rig::start("fetch-ref-in-place").await;
    let main = rig.git(&["rev-parse", "HEAD"]);
    let params = SessionNewParams {
        fetch_ref: Some(FetchRefRequest {
            source_ref: SOURCE_REF.to_owned(),
            commit: main,
        }),
        ..session_params_for_agent(&RuntimeRef::shell(), rig.repo.cwd().to_path_buf())
    };

    let error = err_payload(rig.new_session(params).await);

    assert_eq!(error.code, "bad_request", "{}", error.msg);
    assert!(rig.sessions().await.is_empty(), "no session record is left");
    rig.stop().await;
}

/// Row 11 of the case table: a retry with the same key replays:
/// `replayed: true` without the `fetched_ref` of the first response.
#[tokio::test]
async fn a_keyed_retry_replays_without_the_fetched_ref() {
    let mut rig = Rig::start("fetch-ref-replay").await;
    let expected = rig.origin_source_ref();
    let mut params = rig.params(SOURCE_REF, &expected);
    params.client_request_id = Some("fetch-ref-key-1".to_owned());

    let (first, fetched) = started(rig.new_session(params.clone()).await);
    assert_eq!(fetched.commit, expected);
    assert_eq!(fetched.source_ref, SOURCE_REF);

    let result: SessionNewResult =
        serde_json::from_value(ok_payload(rig.new_session(params.clone()).await))
            .expect("session.new replay");
    assert_eq!(result.replayed, Some(true));
    assert_eq!(result.session.id, first.id);
    assert_eq!(result.fetched_ref, None, "a replay carries no fetched_ref");

    // The same key with another commit names a different request, so the
    // daemon refuses with `session_request_conflict` and creates nothing.
    let mut conflict = params.clone();
    conflict.fetch_ref = Some(FetchRefRequest {
        source_ref: SOURCE_REF.to_owned(),
        commit: rig.git(&["rev-parse", "HEAD"]),
    });
    let error = err_payload(rig.new_session(conflict).await);
    assert_eq!(error.code, "session_request_conflict", "{}", error.msg);
    assert_eq!(
        rig.sessions().await.len(),
        1,
        "the conflict creates no second session"
    );
    rig.stop().await;
}

/// Row 10 of the case table: two sessions on the same head get two session
/// refs; removing the first keeps the second's pin.
#[tokio::test]
async fn two_sessions_on_the_same_head_keep_independent_refs() {
    let mut rig = Rig::start("fetch-ref-two").await;
    let expected = rig.origin_source_ref();

    let first = rig.params_on_branch(SOURCE_REF, &expected, "feat-one");
    let (one, one_fetched) = started(rig.new_session(first).await);
    let second = rig.params_on_branch(SOURCE_REF, &expected, "feat-two");
    let (two, two_fetched) = started(rig.new_session(second).await);

    assert_ne!(
        one_fetched.fetched_ref, two_fetched.fetched_ref,
        "each session pins its own ref even for the same head"
    );
    assert_eq!(one_fetched.commit, expected);
    assert_eq!(two_fetched.commit, expected);

    let remove = Request::make(
        "fetch-ref-remove-first",
        method::SESSION_REMOVE,
        serde_json::to_value(one.id.0.clone()).expect("serialize session id"),
    );
    let _: SessionRemoveResult =
        serde_json::from_value(ok_payload(exchange(&mut rig.control, &remove).await))
            .expect("session.remove result");
    assert_eq!(
        rig.rev(&one_fetched.fetched_ref),
        None,
        "removing the first session deletes its own ref"
    );
    assert_eq!(
        rig.rev(&two_fetched.fetched_ref),
        Some(expected.clone()),
        "the second session keeps its pin"
    );
    assert_eq!(
        rig.git_in(
            two.worktree_path.as_deref().expect("worktree path"),
            &["rev-parse", "HEAD"],
        ),
        rig.git(&["rev-parse", "HEAD"]),
        "the surviving worktree still stands on its base"
    );
    rig.stop().await;
}

/// Row 5 of the case table: a fetch failure that is not a missing ref is
/// `fetch_ref_failed`, names the source ref, and leaves nothing behind.
/// Pointing `origin` at a path that does not exist fails both the fetch
/// and the `ls-remote` probe without any network.
#[tokio::test]
async fn an_unreachable_origin_fails_the_fetch_without_leaving_anything() {
    let mut rig = Rig::start("fetch-ref-broken-origin").await;
    rig.git(&[
        "remote",
        "set-url",
        "origin",
        "/nonexistent-pohunek-fetch-remote",
    ]);
    let expected = rig.git(&["rev-parse", "HEAD"]);
    let error = err_payload(rig.new_session(rig.params(SOURCE_REF, &expected)).await);
    assert_eq!(error.code, "fetch_ref_failed", "{}", error.msg);
    assert!(
        error.msg.contains(SOURCE_REF),
        "the error names the source ref: {}",
        error.msg
    );
    rig.assert_nothing_left().await;
    rig.stop().await;
}

/// Row 7 of the case table: a source ref outside `refs/` and a short
/// commit are `bad_request` before anything is created.
#[tokio::test]
async fn a_malformed_fetch_ref_is_rejected() {
    let mut rig = Rig::start("fetch-ref-malformed").await;
    for (source_ref, commit) in [
        ("pull/42/head", "0123456789abcdef0123456789abcdef01234567"),
        // Passes the `refs/` prefix check but fails `git check-ref-format`
        // (a colon is not a valid ref character), so only the strict check
        // rejects it.
        (
            "refs/pull/1/head:refs/heads/main",
            "0123456789abcdef0123456789abcdef01234567",
        ),
        (SOURCE_REF, "abc123"),
        (SOURCE_REF, "0123456789ABCDEF0123456789ABCDEF01234567"),
    ] {
        let error = err_payload(rig.new_session(rig.params(source_ref, commit)).await);
        assert_eq!(error.code, "bad_request", "{source_ref}: {}", error.msg);
    }
    rig.assert_nothing_left().await;
    rig.stop().await;
}

/// Row 12 of the case table, first half: the extra fetch composes with
/// `branch_source: remote` — the checkout follows the branch's remote head
/// while the session ref pins the unrelated pull-request commit.
#[tokio::test]
async fn fetch_ref_composes_with_a_remote_branch_source() {
    let mut rig = Rig::start("fetch-ref-composed").await;
    let pr_commit = rig.origin_source_ref();
    let remote_head = rig.origin_publish_ref("refs/heads/remote-feature", "remote-feature.txt");

    // `branch_source: remote` draws the checkout branch from `origin`, so
    // the session needs its own branch name pushed there first.
    let mut params = rig.params(SOURCE_REF, &pr_commit);
    params.branch = Some("remote-feature".to_owned());
    params.branch_source = Some(BranchSource::Remote);
    let (session, fetched) = started(rig.new_session(params).await);

    assert_eq!(fetched.commit, pr_commit);
    assert_eq!(fetched.source_ref, SOURCE_REF);
    assert_eq!(
        rig.rev(&fetched.fetched_ref),
        Some(pr_commit.clone()),
        "the session ref holds the pull-request commit, not the branch head"
    );
    let worktree = session.worktree_path.as_deref().expect("worktree path");
    assert_eq!(
        rig.git_in(worktree, &["rev-parse", "HEAD"]),
        remote_head,
        "the checkout stands on the remote branch head"
    );
    rig.stop().await;
}

/// Row 9 of the case table: a required `post-create` hook that fails after
/// promotion compensates the create, so the session ref is gone with the
/// binding. The marker proves the hook ran after the session ref was
/// promoted.
#[tokio::test]
async fn a_failing_required_post_create_hook_compensates_the_session_ref() {
    let marker_dir = temp_dir("fetch-ref-required-hook-marker");
    let marker = marker_dir.join("post-create-ran");
    let marker_body = format!(
        "#!/bin/sh\ntouch {}\necho hook refused >&2\nexit 1\n",
        marker.display()
    );
    let mut rig = Rig::start_with_hooks(
        "fetch-ref-req-compensates",
        None,
        Some(&marker_body),
        vec![RequiredHook::PostCreate],
    )
    .await;
    let expected = rig.origin_source_ref();
    let error = err_payload(rig.new_session(rig.params(SOURCE_REF, &expected)).await);

    assert_eq!(error.code, "hook_required_failed", "{}", error.msg);
    assert!(
        marker.exists(),
        "the hook ran (so after promotion) before the create was refused"
    );
    rig.assert_nothing_left().await;
    rig.stop().await;
}

/// Row 12 of the case table, second half: a `pre-create` hook runs after
/// the extra fetch but before promotion, so the session ref is not there
/// yet — the hook recording an empty resolve pins the order.
#[tokio::test]
async fn the_pre_create_hook_runs_before_promotion() {
    let mut rig = Rig::start("fetch-ref-pre-sees").await;
    let expected = rig.origin_source_ref();
    let seen_dir = temp_dir("fetch-ref-hook-seen");
    let seen = seen_dir.join("seen");
    // The hook runs in the repository checkout with a cleared environment,
    // so it records through a path baked into its body. The guard lives
    // for the whole scenario, so the directory cannot leak into TMPDIR.
    rig.in_repo_pre_create_hook(&seen);

    let (session, fetched) = started(rig.new_session(rig.params(SOURCE_REF, &expected)).await);

    assert!(
        std::fs::read_to_string(&seen)
            .expect("the pre-create hook recorded the session ref")
            .trim()
            .is_empty(),
        "the pre-create hook ran before promotion, so the session ref is not there yet"
    );
    assert_eq!(
        rig.rev(&fetched.fetched_ref),
        Some(expected.clone()),
        "the session ref holds the expected commit after the hooks ran"
    );
    assert_eq!(
        rig.git_in(
            session.worktree_path.as_deref().expect("worktree path"),
            &["rev-parse", "--abbrev-ref", "HEAD"]
        ),
        "feat-fetch",
        "the worktree checks out the requested branch"
    );
    rig.stop().await;
}

/// The no-submodule-recursion probe: the `fetch_ref` fetch must not recurse
/// into submodules (the daemon passes `--no-recurse-submodules`). The
/// fixture records `submodule.recurse=true` like an owner's recursive
/// checkout and populates the submodule, moves the gitlink to v2, and resets
/// `.git/modules/sub`'s `origin/main` to v1 before the daemon fetch — so
/// a recursing fetch would move it back to v2 while this fetch must
/// leave it at v1. The parent store never holds v2; only the
/// submodule's own store does.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "the row-5 scenario builds the two-commit submodule origin, the v1 gitlink, the populated store and the move commit in one flow"
)]
async fn fetch_ref_does_not_recurse_into_submodules() {
    let mut rig = Rig::start("fetch-ref-submodule").await;
    // A submodule whose origin has two commits of which the parent records
    // only v1: `sub.git` holds both, the parent's own store never learns
    // either. The fixture-local `file://` url keeps submodule clones
    // in-process; the daemon's `fetch_ref` fetch still goes to `origin`,
    // never to this url.
    let sub_src = temp_dir("fetch-ref-sub-src");
    let sub_bare = temp_dir("fetch-ref-sub-bare");
    let sub_src_path = sub_src.join("sub");
    let sub_bare_path = sub_bare.join("sub.git");
    for (dir, args) in [
        (&sub_src_path, vec!["init", "-q", "-b", "main", "."]),
        (&sub_bare_path, vec!["init", "-q", "--bare", "."]),
    ] {
        std::fs::create_dir_all(dir).expect("create the submodule repositories");
        let out = rig.git_raw(dir, &args);
        assert!(
            out.status.success(),
            "init the submodule repositories: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // Signed commits (a developer's `commit.gpgsign=true` reaches test git
    // through the repository-local config) fail here, so the submodule
    // commits stay unsigned like every other repo here.
    rig.git_in(&sub_src_path, &["config", "commit.gpgsign", "false"]);
    std::fs::write(sub_src_path.join("sub.txt"), "v1\n").expect("write the submodule file");
    rig.git_in(&sub_src_path, &["add", "."]);
    rig.git_in(&sub_src_path, &["commit", "-q", "-m", "sub v1"]);
    let sub_url = format!("file://{}", sub_bare_path.display());
    rig.git_in(&sub_src_path, &["remote", "add", "origin", &sub_url]);
    rig.git_in(
        &sub_src_path,
        &["push", "-q", "origin", "HEAD:refs/heads/main"],
    );
    let sub_v1 = rig.git_in(&sub_src_path, &["rev-parse", "HEAD"]);
    std::fs::write(sub_src_path.join("sub.txt"), "v2\n").expect("write the submodule update");
    rig.git_in(&sub_src_path, &["commit", "-q", "-am", "sub v2"]);
    let sub_v2 = rig.git_in(&sub_src_path, &["rev-parse", "HEAD"]);
    rig.git_in(
        &sub_src_path,
        &["push", "-q", "origin", "HEAD:refs/heads/main"],
    );
    assert!(
        rig.repo
            .git_command()
            .arg(format!("--git-dir={}", sub_bare_path.display()))
            .args(["symbolic-ref", "HEAD", "refs/heads/main"])
            .output()
            .expect("point the submodule bare HEAD at main")
            .status
            .success(),
        "point the submodule origin at main"
    );
    // Register the submodule at v1 without checking it out: a later
    // `update --init` on a missing path would clone v2's head, so only
    // the gitlink and `.gitmodules` are recorded for now. The parent store
    // learns neither v1 nor v2 — the gitlink is written through plumbing,
    // not through a checkout.
    rig.git_in(
        rig.repo.cwd(),
        &[
            "config",
            "--file",
            ".gitmodules",
            "submodule.sub.path",
            "sub",
        ],
    );
    rig.git_in(
        rig.repo.cwd(),
        &[
            "config",
            "--file",
            ".gitmodules",
            "submodule.sub.url",
            &sub_url,
        ],
    );
    rig.git_in(
        rig.repo.cwd(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000",
            &sub_v1,
            "sub",
        ],
    );
    // The worktree has no `sub` path, so the `update-index` entry above
    // stages the gitlink while `add` can only stage `.gitmodules`: `commit`
    // then records both from the index together.
    rig.git(&["add", ".gitmodules"]);
    rig.git_in(
        rig.repo.cwd(),
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "record the submodule at v1",
        ],
    );
    assert_eq!(
        rig.git(&["rev-parse", "HEAD:sub"]),
        sub_v1,
        "the parent records the submodule at v1"
    );
    rig.git(&["push", "-q", "origin", "HEAD:refs/heads/main"]);
    // Four moving parts: a populated submodule checkout (as after an
    // owner's recursive clone), `submodule.recurse=true` in the repo
    // config, `.git/modules/sub`'s `origin/main` reset to v1, and the
    // parent head moved to the v2 gitlink. A recursing `fetch_ref` fetch
    // would move the probe back to v2; the flag under test keeps it at v1.
    // The `-c protocol.file.allow=always` knob stays scoped to these two
    // `file://` calls: without it the hermetic git refuses both the
    // `update --init` clone and the daemon's own fetch below.
    rig.git_in(
        rig.repo.cwd(),
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "-q",
        ],
    );
    rig.git(&["config", "submodule.recurse", "true"]);
    rig.git(&["checkout", "-q", "-b", "base"]);
    // Probes `-C` into the submodule checkout: git discovers
    // `.git/modules/sub` from the worktree.
    let submodule_main = |rig: &Rig| {
        String::from_utf8_lossy(
            &rig.git_raw(
                &rig.repo.cwd().join("sub"),
                &["rev-parse", "refs/remotes/origin/main"],
            )
            .stdout,
        )
        .trim()
        .to_owned()
    };
    // `update --init` cloned the submodule at v2, so the store tracks v2:
    // the mutation probe below is not vacuous.
    assert_eq!(
        submodule_main(&rig),
        sub_v2,
        "the submodule store tracks its origin at v2"
    );
    assert!(
        !rig.git_raw(
            rig.repo.cwd(),
            &["cat-file", "-e", &format!("{sub_v2}^{{commit}}")]
        )
        .status
        .success(),
        "the parent store has no submodule commit yet"
    );
    rig.git_in(
        rig.repo.cwd(),
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "sync",
            "-q",
        ],
    );
    // Move the gitlink to v2 without pushing the submodule along:
    // `push.recurseSubmodules=no` keeps `origin`'s v2 objects out of the
    // fetch path, so only the `fetch_ref` fetch under test can move the
    // probe. Publish the move commit on `origin` under the source ref and
    // reset the probe to v1: without `--no-recurse-submodules` the fetch
    // moves it back to v2.
    rig.git_in(&rig.repo.cwd().join("sub"), &["fetch", "-q", "origin"]);
    rig.git_in(&rig.repo.cwd().join("sub"), &["checkout", "-q", &sub_v2]);
    rig.git(&["add", "sub"]);
    rig.git_in(
        rig.repo.cwd(),
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "move the submodule to v2",
        ],
    );
    assert_eq!(
        rig.git(&["rev-parse", "HEAD:sub"]),
        sub_v2,
        "the move commit records the submodule at v2"
    );
    let expected = rig.git(&["rev-parse", "HEAD"]);
    // The move commit carries the v2 gitlink, so the push must not recurse
    // into submodules; the fixture-local `file://` url is never pushed.
    rig.git(&["update-ref", SOURCE_REF, "HEAD"]);
    rig.git(&[
        "-c",
        "push.recurseSubmodules=no",
        "push",
        "-q",
        "origin",
        &format!("{SOURCE_REF}:{SOURCE_REF}"),
    ]);
    rig.git_in(
        &rig.repo.cwd().join("sub"),
        &["update-ref", "refs/remotes/origin/main", &sub_v1],
    );
    assert_eq!(
        submodule_main(&rig),
        sub_v1,
        "the submodule origin/main points at v1 before the fetch"
    );

    let (session, fetched) = started(rig.new_session(rig.params(SOURCE_REF, &expected)).await);
    assert_eq!(fetched.commit, expected);

    // The parent's own commit arrived in the repo's object store, but the
    // submodule's `origin/main` did not move and v2 stayed out of the
    // parent store: a recursing fetch would have updated both.
    let worktree = session.worktree_path.as_deref().expect("worktree path");
    assert!(
        rig.git_raw(
            rig.repo.cwd(),
            &["cat-file", "-e", &format!("{expected}^{{commit}}")]
        )
        .status
        .success(),
        "the fetched parent commit exists in the repository"
    );
    assert_eq!(
        submodule_main(&rig),
        sub_v1,
        "the submodule origin/main still points at v1 after a non-recursing fetch"
    );
    // v2 reached the *submodule* store through the fixture's own
    // `sync`/`fetch` above (that is how the gitlink could be moved); the
    // non-recursion claim is about the `fetch_ref` fetch itself, which
    // must not have *moved* `origin/main` there. The parent store — the
    // one the daemon fetched into — still lacks v2.
    assert!(
        !rig.git_raw(
            rig.repo.cwd(),
            &["cat-file", "-e", &format!("{sub_v2}^{{commit}}")]
        )
        .status
        .success(),
        "the submodule v2 commit stayed out of the parent store"
    );
    // The worktree starts from the move commit the daemon just fetched,
    // not from the scenario's base tip: the checkout's gitlink at v2
    // keeps the session reproducible.
    assert_eq!(
        rig.git_in(worktree, &["rev-parse", "HEAD:sub"]),
        sub_v2,
        "the checkout starts from the fetched move commit"
    );
    rig.stop().await;
}
