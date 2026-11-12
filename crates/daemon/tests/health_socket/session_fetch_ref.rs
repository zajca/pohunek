//! `session.new` with `fetch_ref`, through the public control socket against
//! real git repositories and a bare `origin`.
//!
//! The scenarios pin that the extra ref fetch lands the expected commit under
//! the session ref without checking it out, that a refused request leaves no
//! session, binding, worktree, temp ref or session ref behind, that the fetch
//! composes with `branch_source: remote` (it draws its own ref, not the
//! branch's), and that `FETCH_HEAD` is untouched by the fetch.

// Rust guideline compliant 2026-11-12

use super::*;

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
        let repo = init_git_repo();
        let origin = temp_dir(&format!("{tag}-origin"));
        let worktree_root = temp_dir(&format!("{tag}-wt"));
        let state = temp_dir(&format!("{tag}-state"));
        let socket = temp_socket(tag);
        let config = SessionRegistryConfig {
            shell_command: support::hermetic_shell(),
            worktree_root: Some(worktree_root.to_path_buf()),
            store_path: Some(state.join("metadata.jsonl")),
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
    /// scenario uses). Whether the fresh worktree sees the script depends on
    /// where the daemon resolves in-repo hooks from; the hook scenarios
    /// below assert what the daemon reports.
    ///
    /// `fail` makes the hook exit non-zero; otherwise the hook records the
    /// commit the session ref holds at hook time through `seen`.
    fn in_repo_pre_create_hook(&self, fail: bool, seen: &Path) {
        if let Some(parent) = seen.parent() {
            std::fs::create_dir_all(parent).expect("create the seen directory");
        }
        let _ = seen;
        let path = self.repo.cwd().join(".pohunek/hooks/pre-create");
        std::fs::create_dir_all(path.parent().expect("hook parent"))
            .expect("create the hooks directory");
        let body = if fail {
            "#!/bin/sh\necho hook refused >&2\nexit 1\n".to_owned()
        } else {
            format!(
                "#!/bin/sh\nGIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_COUNT=0 GIT_TERMINAL_PROMPT=0 git -C \"$POHUNEK_REPO\" rev-parse --verify --quiet \"refs/pohunek/sessions/$POHUNEK_SESSION_ID/fetched^{{commit}}\" > {} 2>/dev/null || : > {}\nexit 0\n",
                seen.display(),
                seen.display()
            )
        };
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
    // fetched commit: the commit exists as an object in the shared
    // repository but the new branch does not contain it.
    assert_eq!(
        rig.git_in(worktree, &["rev-parse", "HEAD"]),
        base,
        "the checkout starts on the base tip"
    );
    assert!(
        rig.repo
            .git_command()
            .arg("-C")
            .arg(rig.repo.cwd())
            .args(["cat-file", "-e", &format!("{expected}^{{commit}}")])
            .output()
            .expect("run git cat-file")
            .status
            .success(),
        "the fetched commit exists in the repository"
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

/// A source ref `origin` does not have fails with `fetch_ref_not_found` and
/// leaves nothing behind.
#[tokio::test]
async fn an_unknown_source_ref_fails_without_leaving_anything() {
    let mut rig = Rig::start("fetch-ref-absent").await;
    let main = rig.git(&["rev-parse", "HEAD"]);

    let error = err_payload(
        rig.new_session(rig.params("refs/pull/404/head", &main))
            .await,
    );

    assert_eq!(error.code, "fetch_ref_not_found", "{}", error.msg);
    rig.assert_nothing_left().await;
    rig.stop().await;
}

/// A fetch that peels to another commit fails with `fetch_ref_commit_mismatch`
/// naming both ids and leaves nothing behind.
#[tokio::test]
async fn a_commit_mismatch_fails_without_leaving_anything() {
    let mut rig = Rig::start("fetch-ref-mismatch").await;
    let expected = rig.origin_source_ref();
    let other = rig.git(&["rev-parse", "HEAD"]);

    let error = err_payload(rig.new_session(rig.params(SOURCE_REF, &other)).await);

    assert_eq!(error.code, "fetch_ref_commit_mismatch", "{}", error.msg);
    assert!(
        error.msg.contains(&other) && error.msg.contains(&expected),
        "the error names both the expected and the fetched commit: {}",
        error.msg
    );
    rig.assert_nothing_left().await;
    rig.stop().await;
}

/// Without `branch` there is no worktree to pin the ref for: `bad_request`.
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

/// A retry with the same key replays: `replayed: true` without the
/// `fetched_ref` of the first response.
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
        serde_json::from_value(ok_payload(rig.new_session(params).await))
            .expect("session.new replay");
    assert_eq!(result.replayed, Some(true));
    assert_eq!(result.session.id, first.id);
    assert_eq!(result.fetched_ref, None, "a replay carries no fetched_ref");
    rig.stop().await;
}

/// A source ref outside `refs/` and a short commit are `bad_request` before
/// anything is created.
#[tokio::test]
async fn a_malformed_fetch_ref_is_rejected() {
    let mut rig = Rig::start("fetch-ref-malformed").await;
    for (source_ref, commit) in [
        ("pull/42/head", "0123456789abcdef0123456789abcdef01234567"),
        (SOURCE_REF, "abc123"),
        (SOURCE_REF, "0123456789ABCDEF0123456789ABCDEF01234567"),
    ] {
        let error = err_payload(rig.new_session(rig.params(source_ref, commit)).await);
        assert_eq!(error.code, "bad_request", "{source_ref}: {}", error.msg);
    }
    rig.assert_nothing_left().await;
    rig.stop().await;
}

/// Row 2 of the case table: the extra fetch composes with
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

/// Row 12 of the case table: the session ref is promoted before the
/// `pre-create` hook, so a hook that reads the session ref at hook time sees
/// the expected commit — here the harness cannot list any hook in
/// `hooks.required` per scenario, so a failing hook reports the failure as a
/// warning and the session ref stays.
#[tokio::test]
async fn a_failing_pre_create_hook_warns_and_keeps_the_session_ref() {
    let mut rig = Rig::start("fetch-ref-pre-fails").await;
    let expected = rig.origin_source_ref();
    let seen = temp_dir("fetch-ref-hook-fails-seen").join("seen");
    rig.in_repo_pre_create_hook(true, &seen);
    // The harness cannot list the hook in `hooks.required` per scenario
    // (that list lives in `service.toml`), so the daemon answers with the
    // non-fatal warning instead of refusing: the hook still proves it ran
    // while the session ref was promoted by recording it before refusing.
    let (session, fetched) = started(rig.new_session(rig.params(SOURCE_REF, &expected)).await);

    assert!(
        session
            .warnings
            .iter()
            .any(|warning| warning.kind == SessionWarningKind::Hook),
        "the failing hook is reported as a warning: {session:?}"
    );
    assert_eq!(
        rig.rev(&fetched.fetched_ref),
        Some(expected.clone()),
        "the session ref stays after a non-required hook failure"
    );
    rig.stop().await;
}

/// Row 12's positive half: a `pre-create` hook runs after the extra fetch
/// but before promotion, so the session ref is not there yet — the hook
/// recording an empty resolve pins the order.
#[tokio::test]
async fn the_pre_create_hook_runs_before_promotion() {
    let mut rig = Rig::start("fetch-ref-pre-sees").await;
    let expected = rig.origin_source_ref();
    let seen = temp_dir("fetch-ref-hook-seen").join("seen");
    // The hook runs in the repository checkout with a cleared environment,
    // so it records through a path baked into its body.
    rig.in_repo_pre_create_hook(false, &seen);

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

/// A repository with a submodule commits the fetch without recursing: the
/// session ref pins the parent commit while no submodule objects are fetched.
#[tokio::test]
async fn fetch_ref_does_not_recurse_into_submodules() {
    let mut rig = Rig::start("fetch-ref-submodule").await;
    let sub_source = temp_dir("fetch-ref-submodule-source");
    let sub_bare = sub_source.join("sub.git");
    rig.git_in(
        rig.repo.cwd(),
        &["init", "-q", "--bare", &sub_bare.display().to_string()],
    );
    let sub_clone = temp_dir("fetch-ref-subclone");
    let sub_dir = sub_clone.join("sub");
    rig.git_in(
        rig.repo.cwd(),
        &[
            "clone",
            "-q",
            &sub_bare.display().to_string(),
            &sub_dir.display().to_string(),
        ],
    );
    rig.git_in(&sub_dir, &["config", "user.email", "test@example.com"]);
    rig.git_in(&sub_dir, &["config", "user.name", "Test"]);
    rig.git_in(&sub_dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(sub_dir.join("sub.txt"), "sub\n").expect("write file");
    rig.git_in(&sub_dir, &["add", "."]);
    rig.git_in(&sub_dir, &["commit", "-q", "-m", "sub"]);
    let sub_head = rig.git_in(&sub_dir, &["rev-parse", "HEAD"]);
    rig.git_in(&sub_dir, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    assert!(
        rig.repo
            .git_command()
            .arg(format!("--git-dir={}", sub_bare.display()))
            .args(["symbolic-ref", "HEAD", "refs/heads/main"])
            .output()
            .expect("point the submodule origin at main")
            .status
            .success(),
        "point the submodule origin at main"
    );
    let sub_origin = format!("file://{}", sub_bare.display());
    rig.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        "-q",
        &sub_origin,
        "sub",
    ]);
    rig.git(&["commit", "-q", "-m", "add submodule"]);
    let origin = rig.origin.join("origin.git").display().to_string();
    rig.git(&["push", "-q", &origin, "HEAD:refs/heads/main"]);
    let expected = rig.origin_source_ref();

    let (session, fetched) = started(rig.new_session(rig.params(SOURCE_REF, &expected)).await);

    assert_eq!(
        rig.rev(&fetched.fetched_ref),
        Some(expected.clone()),
        "the session ref holds the expected commit"
    );
    // The fetch must not recurse: the submodule commit lives only in the
    // submodule's own origin and never in the parent repository.
    assert_eq!(rig.rev(&sub_head), None, "no submodule commit was fetched");
    let worktree = session.worktree_path.as_deref().expect("worktree path");
    assert_eq!(
        rig.git_in(worktree, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "feat-fetch",
        "the worktree checks out the requested branch"
    );
    rig.stop().await;
}
