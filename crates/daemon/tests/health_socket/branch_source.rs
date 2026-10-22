//! `session.new` with `branch_source: remote`, through the public control
//! socket against real git repositories and a bare `origin`.
//!
//! The scenarios pin which commit the worktree starts at for every relation
//! between the local branch and `origin/<branch>`, that a refused request
//! leaves nothing behind, and that no local commit becomes unreachable without
//! a backup ref.

// Rust guideline compliant 2026-10-22

use super::*;

use protocol::{BranchSource, OnDiverged, SessionNewResult, WorktreeSource};

/// The branch every scenario starts a session on.
const BRANCH: &str = "feat";

/// Hex digits of the local head in a backup ref name.
const BACKUP_SHA_LEN: usize = 12;

/// A started daemon, a working repository and the bare `origin` it is cloned
/// from.
struct Rig {
    control: Framed<UnixStream, LinesCodec>,
    shutdown: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
    repo: TestEnv,
    origin: TestDir,
    worktree_root: TestDir,
    main: String,
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
            main: String::new(),
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
        let main = rig.git(&["rev-parse", "HEAD"]);
        Self { main, ..rig }
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = self.server.await;
    }

    fn origin_path(&self) -> PathBuf {
        self.origin.join("origin.git")
    }

    /// Runs git in `dir` with the scrubbed environment and returns trimmed stdout.
    fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        let out = self
            .repo
            .command("git")
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
            .command("git")
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

    /// Commits a new file on top of `origin/<branch>` (or `origin`'s default
    /// branch) from a fresh clone and force-pushes it as `branch`.
    fn origin_commit(&self, branch: &str, file: &str) -> String {
        let clone = temp_dir("origin-clone");
        let dir = clone.join("clone");
        let origin = self.origin_path().display().to_string();
        self.git_in(
            self.repo.cwd(),
            &["clone", "-q", &origin, &dir.display().to_string()],
        );
        self.git_in(&dir, &["config", "user.email", "test@example.com"]);
        self.git_in(&dir, &["config", "user.name", "Test"]);
        self.git_in(&dir, &["config", "commit.gpgsign", "false"]);
        let start = format!("origin/{branch}");
        let base = if self
            .repo
            .command("git")
            .arg("-C")
            .arg(&dir)
            .args(["rev-parse", "--verify", "--quiet", &start])
            .output()
            .expect("probe origin branch")
            .status
            .success()
        {
            start
        } else {
            "origin/main".to_owned()
        };
        self.git_in(&dir, &["checkout", "-q", "-B", branch, &base]);
        std::fs::write(dir.join(file), format!("{file}\n")).expect("write file");
        self.git_in(&dir, &["add", "."]);
        self.git_in(&dir, &["commit", "-q", "-m", file]);
        self.git_in(
            &dir,
            &[
                "push",
                "-q",
                "--force",
                "origin",
                &format!("HEAD:refs/heads/{branch}"),
            ],
        );
        self.git_in(&dir, &["rev-parse", "HEAD"])
    }

    /// Creates the local `branch` at `start` after fetching `origin`.
    fn local_branch(&self, branch: &str, start: &str) {
        self.git(&["fetch", "-q", "origin"]);
        self.git(&["branch", branch, start]);
    }

    /// Commits a new file on the local `branch`, leaving `main` checked out.
    fn local_commit(&self, branch: &str, file: &str) -> String {
        self.git(&["checkout", "-q", branch]);
        std::fs::write(self.repo.cwd().join(file), format!("{file}\n")).expect("write file");
        self.git(&["add", "."]);
        self.git(&["commit", "-q", "-m", file]);
        let head = self.git(&["rev-parse", "HEAD"]);
        self.git(&["checkout", "-q", "main"]);
        head
    }

    fn params(
        &self,
        branch_source: Option<BranchSource>,
        on_diverged: Option<OnDiverged>,
    ) -> SessionNewParams {
        let mut params = session_params_for_worktree(
            &RuntimeRef::shell(),
            self.repo.cwd().to_path_buf(),
            BRANCH,
        );
        params.branch_source = branch_source;
        params.on_diverged = on_diverged;
        params
    }

    async fn new_session(&mut self, params: SessionNewParams) -> Response {
        create_session_with_params(&mut self.control, params).await
    }

    async fn remote_session(&mut self, on_diverged: Option<OnDiverged>) -> Response {
        let params = self.params(Some(BranchSource::Remote), on_diverged);
        self.new_session(params).await
    }

    async fn sessions(&mut self) -> Vec<SessionInfo> {
        let list = Request::make(
            "branch-source-list",
            method::SESSION_LIST,
            serde_json::to_value(SessionListParams::default()).expect("serialize list params"),
        );
        serde_json::from_value(ok_payload(exchange(&mut self.control, &list).await))
            .expect("session list")
    }

    /// Asserts that a refused request left no session, worktree or private
    /// fetch ref, and that the worktree root is empty when `root_empty`.
    async fn assert_nothing_left(&mut self, root_empty: bool) {
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
        self.assert_no_fetch_refs();
        if root_empty {
            let leftover: Vec<_> = std::fs::read_dir(&*self.worktree_root)
                .expect("read the worktree root")
                .collect();
            assert!(
                leftover.is_empty(),
                "no worktree directory is left: {leftover:?}"
            );
        }
    }

    fn assert_no_fetch_refs(&self) {
        assert_eq!(
            self.git(&["for-each-ref", "refs/pohunek/fetch"]),
            "",
            "the private fetch ref is deleted"
        );
    }

    fn assert_no_backup_refs(&self) {
        assert_eq!(
            self.git(&["for-each-ref", "refs/heads/backup"]),
            "",
            "no backup ref exists"
        );
    }

    fn backup_name(local: &str) -> String {
        format!("backup/{BRANCH}-{}", &local[..BACKUP_SHA_LEN])
    }
}

fn started(response: Response) -> (SessionInfo, Option<WorktreeSource>) {
    let result: SessionNewResult =
        serde_json::from_value(ok_payload(response)).expect("session.new result");
    (result.session, result.worktree_source)
}

fn source(source_ref: &str, commit: &str) -> WorktreeSource {
    WorktreeSource {
        source_ref: source_ref.to_owned(),
        commit: commit.to_owned(),
        fast_forwarded_from: None,
        backup_ref: None,
    }
}

fn worktree_head(rig: &Rig, session: &SessionInfo) -> String {
    rig.git_in(
        session.worktree_path.as_deref().expect("worktree path"),
        &["rev-parse", "HEAD"],
    )
}

#[tokio::test]
async fn a_branch_behind_origin_is_fast_forwarded_and_reported() {
    let mut rig = Rig::start("bs-behind").await;
    let remote = rig.origin_commit(BRANCH, "remote.txt");
    let main = rig.main.clone();
    rig.local_branch(BRANCH, &main);

    // `on_diverged` has no effect on a branch that did not diverge.
    let (session, worktree_source) =
        started(rig.remote_session(Some(OnDiverged::BackupReset)).await);

    assert_eq!(
        worktree_source,
        Some(WorktreeSource {
            fast_forwarded_from: Some(main),
            ..source("origin/feat", &remote)
        })
    );
    assert_eq!(worktree_head(&rig, &session), remote);
    assert_eq!(rig.rev("refs/heads/feat"), Some(remote));
    rig.assert_no_backup_refs();
    rig.assert_no_fetch_refs();
    rig.stop().await;
}

#[tokio::test]
async fn a_missing_local_branch_is_created_at_the_remote_head() {
    let mut rig = Rig::start("bs-missing").await;
    let remote = rig.origin_commit(BRANCH, "remote.txt");

    let (session, worktree_source) =
        started(rig.remote_session(Some(OnDiverged::BackupReset)).await);

    assert_eq!(worktree_source, Some(source("origin/feat", &remote)));
    assert_eq!(worktree_head(&rig, &session), remote);
    assert_eq!(rig.rev("refs/heads/feat"), Some(remote));
    rig.assert_no_backup_refs();
    rig.assert_no_fetch_refs();
    rig.stop().await;
}

#[tokio::test]
async fn a_branch_equal_to_origin_is_used_as_it_is() {
    let mut rig = Rig::start("bs-equal").await;
    let remote = rig.origin_commit(BRANCH, "remote.txt");
    rig.local_branch(BRANCH, "origin/feat");

    let (session, worktree_source) =
        started(rig.remote_session(Some(OnDiverged::BackupReset)).await);

    assert_eq!(worktree_source, Some(source("origin/feat", &remote)));
    assert_eq!(worktree_head(&rig, &session), remote);
    rig.assert_no_backup_refs();
    rig.assert_no_fetch_refs();
    rig.stop().await;
}

#[tokio::test]
async fn a_branch_ahead_of_origin_keeps_its_commits_and_warns() {
    let mut rig = Rig::start("bs-ahead").await;
    let remote = rig.origin_commit(BRANCH, "remote.txt");
    rig.local_branch(BRANCH, "origin/feat");
    let local = rig.local_commit(BRANCH, "local.txt");

    let (session, worktree_source) =
        started(rig.remote_session(Some(OnDiverged::BackupReset)).await);

    assert_eq!(worktree_source, Some(source("refs/heads/feat", &local)));
    rig.assert_no_backup_refs();
    assert_eq!(worktree_head(&rig, &session), local);
    assert_eq!(rig.rev("refs/heads/feat"), Some(local.clone()));
    let warning = session
        .warnings
        .iter()
        .find(|warning| warning.kind == SessionWarningKind::BranchAheadOfRemote)
        .expect("a branch_ahead_of_remote warning");
    let detail = warning.detail.as_deref().expect("warning detail");
    assert!(
        detail.contains(&local) && detail.contains(&remote),
        "the warning names both heads: {detail}"
    );
    rig.assert_no_fetch_refs();
    rig.stop().await;
}

#[tokio::test]
async fn a_diverged_branch_is_refused_and_left_untouched() {
    let mut rig = Rig::start("bs-diverged").await;
    let main = rig.main.clone();
    let remote = rig.origin_commit(BRANCH, "remote.txt");
    rig.local_branch(BRANCH, &main);
    let local = rig.local_commit(BRANCH, "local.txt");

    let error = err_payload(rig.remote_session(None).await);

    assert_eq!(error.code, "branch_diverged");
    for sha in [&local, &remote, &main] {
        assert!(
            error.msg.contains(sha.as_str()),
            "the message names {sha}: {}",
            error.msg
        );
    }
    assert_eq!(rig.rev("refs/heads/feat"), Some(local));
    rig.assert_no_backup_refs();
    rig.assert_nothing_left(true).await;
    rig.stop().await;
}

#[tokio::test]
async fn a_diverged_branch_is_backed_up_and_reset_when_asked() {
    let mut rig = Rig::start("bs-backup").await;
    let main = rig.main.clone();
    let remote = rig.origin_commit(BRANCH, "remote.txt");
    rig.local_branch(BRANCH, &main);
    let local = rig.local_commit(BRANCH, "local.txt");

    let (session, worktree_source) =
        started(rig.remote_session(Some(OnDiverged::BackupReset)).await);

    let backup = Rig::backup_name(&local);
    assert_eq!(
        worktree_source,
        Some(WorktreeSource {
            backup_ref: Some(backup.clone()),
            ..source("origin/feat", &remote)
        })
    );
    assert_eq!(worktree_head(&rig, &session), remote);
    assert_eq!(rig.rev("refs/heads/feat"), Some(remote));
    assert_eq!(rig.rev(&format!("refs/heads/{backup}")), Some(local));
    rig.assert_no_fetch_refs();
    rig.stop().await;
}

#[tokio::test]
async fn a_taken_backup_name_refuses_the_reset() {
    let mut rig = Rig::start("bs-backup-taken").await;
    let main = rig.main.clone();
    rig.origin_commit(BRANCH, "remote.txt");
    rig.local_branch(BRANCH, &main);
    let local = rig.local_commit(BRANCH, "local.txt");
    let backup = Rig::backup_name(&local);
    rig.git(&["branch", &backup, &main]);

    let error = err_payload(rig.remote_session(Some(OnDiverged::BackupReset)).await);

    assert_eq!(error.code, "backup_ref_exists");
    assert_eq!(rig.rev("refs/heads/feat"), Some(local));
    assert_eq!(rig.rev(&format!("refs/heads/{backup}")), Some(main));
    rig.assert_nothing_left(true).await;
    rig.stop().await;
}

#[tokio::test]
async fn a_branch_named_backup_blocks_the_backup_namespace() {
    let mut rig = Rig::start("bs-backup-blocked").await;
    let main = rig.main.clone();
    rig.origin_commit(BRANCH, "remote.txt");
    rig.local_branch(BRANCH, &main);
    let local = rig.local_commit(BRANCH, "local.txt");
    // `refs/heads/backup` cannot coexist with `refs/heads/backup/<name>`.
    rig.git(&["branch", "backup", &main]);

    let error = err_payload(rig.remote_session(Some(OnDiverged::BackupReset)).await);

    assert_eq!(error.code, "backup_ref_exists");
    assert_eq!(rig.rev("refs/heads/feat"), Some(local));
    rig.assert_nothing_left(true).await;
    rig.stop().await;
}

#[tokio::test]
async fn a_missing_remote_branch_or_origin_is_refused() {
    let mut rig = Rig::start("bs-not-found").await;
    // A tag of that name on origin is not a branch.
    rig.git_in(&rig.origin_path(), &["tag", BRANCH, "main"]);

    let error = err_payload(rig.remote_session(None).await);
    assert_eq!(error.code, "remote_branch_not_found");
    rig.assert_nothing_left(true).await;

    rig.git(&["remote", "remove", "origin"]);
    let error = err_payload(rig.remote_session(None).await);
    assert_eq!(error.code, "remote_branch_not_found");
    rig.assert_nothing_left(true).await;
    rig.stop().await;
}

#[tokio::test]
async fn an_unreachable_origin_is_a_hard_fetch_error() {
    let mut rig = Rig::start("bs-unreachable").await;
    rig.origin_commit(BRANCH, "remote.txt");
    let missing = rig.origin.join("gone.git").display().to_string();
    rig.git(&["remote", "set-url", "origin", &missing]);

    let error = err_payload(rig.remote_session(None).await);

    assert_eq!(error.code, "branch_fetch_failed");
    rig.assert_nothing_left(true).await;
    rig.stop().await;
}

#[tokio::test]
async fn invalid_option_combinations_are_refused_before_any_side_effect() {
    let mut rig = Rig::start("bs-invalid").await;
    rig.origin_commit(BRANCH, "remote.txt");

    let mut no_branch = rig.params(Some(BranchSource::Remote), None);
    no_branch.branch = None;
    let mut with_base = rig.params(Some(BranchSource::Remote), None);
    with_base.base_branch = Some("main".to_owned());
    let local_reset = rig.params(None, Some(OnDiverged::BackupReset));
    for params in [no_branch, with_base, local_reset] {
        let error = err_payload(rig.new_session(params).await);
        assert_eq!(error.code, "bad_request", "{}", error.msg);
    }

    let mut raw = serde_json::to_value(rig.params(None, None)).expect("serialize params");
    raw["branch_source"] = Value::String("sideways".to_owned());
    let response = exchange(
        &mut rig.control,
        &Request::make("branch-source-unknown", method::SESSION_NEW, raw),
    )
    .await;
    assert_eq!(err_payload(response).code, "bad_request");
    let mut raw = serde_json::to_value(rig.params(Some(BranchSource::Remote), None))
        .expect("serialize params");
    raw["on_diverged"] = Value::String("discard".to_owned());
    let response = exchange(
        &mut rig.control,
        &Request::make("branch-source-unknown-resolution", method::SESSION_NEW, raw),
    )
    .await;
    assert_eq!(err_payload(response).code, "bad_request");

    // A name git rejects fails before any remote work.
    let mut glob = rig.params(Some(BranchSource::Remote), None);
    glob.branch = Some("feat*".to_owned());
    assert_eq!(
        err_payload(rig.new_session(glob).await).code,
        "invalid_branch"
    );

    rig.assert_nothing_left(true).await;
    rig.stop().await;
}

#[tokio::test]
async fn without_the_option_a_stale_local_branch_is_checked_out_as_it_is() {
    let mut rig = Rig::start("bs-local").await;
    let main = rig.main.clone();
    rig.origin_commit(BRANCH, "remote.txt");
    rig.local_branch(BRANCH, &main);

    for source in [None, Some(BranchSource::Local)] {
        let params = rig.params(source, None);
        let (session, worktree_source) = started(rig.new_session(params).await);
        assert_eq!(worktree_source, None);
        assert_eq!(worktree_head(&rig, &session), main);
        let remove = Request::make(
            "branch-source-remove",
            method::SESSION_REMOVE,
            serde_json::to_value(&session.id).expect("serialize id"),
        );
        ok_payload(exchange(&mut rig.control, &remove).await);
    }
    assert_eq!(rig.rev("refs/heads/feat"), Some(main));
    rig.stop().await;
}

#[tokio::test]
async fn a_branch_checked_out_elsewhere_is_not_moved() {
    let mut rig = Rig::start("bs-in-use").await;
    let main = rig.main.clone();
    rig.origin_commit(BRANCH, "remote.txt");
    rig.local_branch(BRANCH, &main);
    rig.git(&["checkout", "-q", BRANCH]);

    let error = err_payload(rig.remote_session(None).await);
    assert_eq!(error.code, "worktree_branch_in_use");
    assert_eq!(rig.rev("refs/heads/feat"), Some(main.clone()));

    // A diverged branch is neither backed up nor reset either.
    rig.git(&["commit", "-q", "--allow-empty", "-m", "local"]);
    let local = rig.git(&["rev-parse", "HEAD"]);
    let error = err_payload(rig.remote_session(Some(OnDiverged::BackupReset)).await);
    assert_eq!(error.code, "worktree_branch_in_use");
    assert_eq!(rig.rev("refs/heads/feat"), Some(local));
    rig.assert_no_backup_refs();
    rig.assert_no_fetch_refs();
    rig.stop().await;
}

/// Installs an uncommitted `pre-create` hook in the working repository.
fn install_pre_create_hook(rig: &Rig, body: &str) {
    let dir = rig.repo.cwd().join(".pohunek/hooks");
    std::fs::create_dir_all(&dir).expect("create the hooks directory");
    std::fs::write(dir.join("pre-create"), body).expect("write the hook");
}

#[tokio::test]
async fn a_branch_that_moves_before_the_swap_is_neither_reset_nor_backed_up() {
    let mut rig = Rig::start("bs-cas").await;
    let main = rig.main.clone();
    rig.origin_commit(BRANCH, "remote.txt");
    rig.local_branch(BRANCH, &main);
    let local = rig.local_commit(BRANCH, "local.txt");
    // The hook runs between the plan and the ref update and moves the branch.
    install_pre_create_hook(
        &rig,
        &format!("#!/bin/sh\ngit -C \"$POHUNEK_REPO\" update-ref refs/heads/{BRANCH} {main}\n"),
    );

    let error = err_payload(rig.remote_session(Some(OnDiverged::BackupReset)).await);

    assert_eq!(error.code, "worktree_add_failed");
    assert!(
        error.msg.contains("moved concurrently") && error.msg.contains("no backup or reset"),
        "{}",
        error.msg
    );
    assert_eq!(
        rig.rev("refs/heads/feat"),
        Some(main),
        "the hook's move stands"
    );
    assert_ne!(rig.rev("refs/heads/feat"), Some(local));
    rig.assert_no_backup_refs();
    rig.assert_nothing_left(true).await;
    rig.stop().await;
}

#[tokio::test]
async fn a_failed_add_after_a_fast_forward_keeps_the_fast_forward() {
    let mut rig = Rig::start("bs-add-fails").await;
    let main = rig.main.clone();
    let remote = rig.origin_commit(BRANCH, "remote.txt");
    rig.local_branch(BRANCH, &main);
    // The hook occupies the target directory, so `git worktree add` fails.
    let repo_slug = pohunek_daemon::worktree::branch_slug(
        &rig.repo
            .cwd()
            .file_name()
            .expect("repository directory name")
            .to_string_lossy(),
    );
    let root = rig.worktree_root.display().to_string();
    install_pre_create_hook(
        &rig,
        &format!(
            "#!/bin/sh\nd=\"{root}/$POHUNEK_SESSION_ID-{repo_slug}-{BRANCH}\"\nmkdir -p \"$d\"\n: > \"$d/blocker\"\n"
        ),
    );

    let error = err_payload(rig.remote_session(None).await);

    assert_eq!(error.code, "worktree_add_failed", "{}", error.msg);
    assert_eq!(
        rig.rev("refs/heads/feat"),
        Some(remote),
        "the fast-forward loses no commit and stays"
    );
    rig.assert_nothing_left(false).await;
    rig.stop().await;
}

#[tokio::test]
async fn a_project_default_base_is_ignored_in_remote_mode() {
    let mut rig = Rig::start("bs-project-base").await;
    let remote = rig.origin_commit(BRANCH, "remote.txt");
    // A default base that exists neither locally nor on origin would fail a
    // local-mode create while resolving and fetching it.
    let add = Request::make(
        "branch-source-project-add",
        method::PROJECT_ADD,
        serde_json::json!({ "path": rig.repo.cwd(), "base_branch": "no-such-base" }),
    );
    let project: ProjectInfo =
        serde_json::from_value(ok_payload(exchange(&mut rig.control, &add).await))
            .expect("project info");
    let mut params = rig.params(Some(BranchSource::Remote), None);
    params.repo = None;
    params.project = Some(project.id);

    let (session, worktree_source) = started(rig.new_session(params).await);

    assert_eq!(worktree_source, Some(source("origin/feat", &remote)));
    assert_eq!(worktree_head(&rig, &session), remote);
    rig.assert_no_fetch_refs();
    rig.stop().await;
}

#[tokio::test]
async fn unrelated_histories_are_diverged_with_no_merge_base() {
    let mut rig = Rig::start("bs-unrelated").await;
    let main = rig.main.clone();
    let clone = temp_dir("bs-orphan");
    let dir = clone.join("clone");
    let origin = rig.origin_path().display().to_string();
    rig.git_in(
        rig.repo.cwd(),
        &["clone", "-q", &origin, &dir.display().to_string()],
    );
    rig.git_in(&dir, &["config", "user.email", "test@example.com"]);
    rig.git_in(&dir, &["config", "user.name", "Test"]);
    rig.git_in(&dir, &["checkout", "-q", "--orphan", BRANCH]);
    rig.git_in(&dir, &["commit", "-q", "--allow-empty", "-m", "orphan"]);
    let remote = rig.git_in(&dir, &["rev-parse", "HEAD"]);
    rig.git_in(
        &dir,
        &["push", "-q", "origin", &format!("HEAD:refs/heads/{BRANCH}")],
    );
    rig.local_branch(BRANCH, &main);

    let error = err_payload(rig.remote_session(None).await);

    assert_eq!(error.code, "branch_diverged");
    assert!(
        error.msg.contains(&remote) && error.msg.contains("merge_base=none"),
        "{}",
        error.msg
    );
    rig.assert_nothing_left(true).await;
    rig.stop().await;
}

#[tokio::test]
async fn a_branch_that_moves_under_an_unswapped_start_is_not_reported_as_planned() {
    let mut rig = Rig::start("bs-moved-equal").await;
    let main = rig.main.clone();
    rig.origin_commit(BRANCH, "remote.txt");
    rig.local_branch(BRANCH, "origin/feat");
    // Equal branches have no compare-and-swap; the hook moves the branch
    // between the plan and the checkout.
    install_pre_create_hook(
        &rig,
        &format!("#!/bin/sh\ngit -C \"$POHUNEK_REPO\" update-ref refs/heads/{BRANCH} {main}\n"),
    );

    let error = err_payload(rig.remote_session(None).await);

    assert_eq!(error.code, "worktree_add_failed");
    assert!(error.msg.contains("moved concurrently"), "{}", error.msg);
    assert_eq!(rig.rev("refs/heads/feat"), Some(main));
    rig.assert_nothing_left(true).await;
    rig.stop().await;
}
