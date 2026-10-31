//! A `post-create` hook listed in `hooks.required`: a failing script aborts
//! `session.new` with `hook_required_failed` and rolls the create back, through
//! the public control socket against real git repositories.

// Rust guideline compliant 2026-10-22

use super::*;

use pohunek_service_config::RequiredHook;

/// Timeout for the hook that must be terminated; short so the scenario is quick.
const SHORT_HOOK_TIMEOUT: Duration = Duration::from_millis(800);

/// Branch every scenario starts a session on.
const BRANCH: &str = "b";

/// A started daemon whose host hooks write their markers into one directory.
struct Rig {
    control: Framed<UnixStream, LinesCodec>,
    shutdown: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
    repo: TestEnv,
    worktree_root: TestDir,
    markers: PathBuf,
    config_dir: PathBuf,
    _socket: TestSocket,
    state: TestDir,
}

/// Registry settings a scenario varies.
struct Settings {
    required: bool,
    timeout: Option<Duration>,
    stderr_tail_bytes: Option<u32>,
}

impl Settings {
    const REQUIRED: Self = Self {
        required: true,
        timeout: None,
        stderr_tail_bytes: None,
    };
}

impl Rig {
    async fn start(tag: &str, settings: Settings) -> Self {
        let repo = init_git_repo();
        let worktree_root = temp_dir(&format!("{tag}-wt"));
        let state = temp_dir(&format!("{tag}-state"));
        let socket = temp_socket(tag);
        let markers = socket.dir.join("markers");
        std::fs::create_dir_all(&markers).expect("create the marker directory");
        let config_dir = socket.dir.join("config");
        std::fs::create_dir_all(config_dir.join("hooks")).expect("create host hook directory");
        let agent = format!(
            "touch '{}'; exec sleep 30",
            markers.join("agent-started").display()
        );
        let mut config = SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", agent.as_str()]),
            worktree_root: Some(worktree_root.to_path_buf()),
            store_path: Some(state.join("metadata.jsonl")),
            config_dir: Some(config_dir.clone()),
            ..SessionRegistryConfig::default()
        };
        if settings.required {
            config.hook_required = vec![RequiredHook::PostCreate];
        }
        if let Some(timeout) = settings.timeout {
            config.hook_timeout = timeout;
        }
        if let Some(bytes) = settings.stderr_tail_bytes {
            config.hook_stderr_tail_bytes = bytes;
        }
        let (shutdown, server, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
        let control = connect(&socket).await;
        let rig = Self {
            control,
            shutdown,
            server,
            repo,
            worktree_root,
            markers,
            config_dir,
            _socket: socket,
            state,
        };
        // Remove hooks and the session-start hook record that they ran.
        for event in ["pre-remove", "post-remove", "session-start"] {
            rig.host_hook(
                event,
                &format!("#!/bin/sh\ntouch '{}'\n", rig.marker(event).display()),
            );
        }
        rig
    }

    fn marker(&self, name: &str) -> PathBuf {
        self.markers.join(name)
    }

    fn host_hook(&self, event: &str, body: &str) {
        write_executable(&self.config_dir.join("hooks").join(event), body);
    }

    /// Commits `.pohunek/<relative>` to the repository, so the worktree has it.
    fn committed_script(&self, relative: &str, body: &str) {
        let path = self.repo.cwd().join(".pohunek").join(relative);
        std::fs::create_dir_all(path.parent().expect("script parent")).expect("create directory");
        write_executable(&path, body);
        for args in [
            vec!["add", "-A"],
            vec!["commit", "-q", "-m", "add a hook script"],
        ] {
            let out = self
                .repo
                .git_command()
                .arg("-C")
                .arg(self.repo.cwd())
                .args(&args)
                .output()
                .expect("run git");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    async fn new_session(&mut self) -> Response {
        create_worktree_session(
            &mut self.control,
            RuntimeRef::shell(),
            self.repo.cwd().to_path_buf(),
            BRANCH,
        )
        .await
    }

    async fn sessions(&mut self) -> Vec<SessionInfo> {
        let list = Request::make("required-list", method::SESSION_LIST, Value::Null);
        serde_json::from_value(ok_payload(exchange(&mut self.control, &list).await))
            .expect("session list")
    }

    fn git(&self, args: &[&str]) -> String {
        let out = self
            .repo
            .git_command()
            .arg("-C")
            .arg(self.repo.cwd())
            .args(args)
            .output()
            .expect("run git");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Asserts that the aborted create left no session, worktree or binding,
    /// ran the remove hooks, and never started the agent or `session-start`.
    async fn assert_rolled_back(&mut self) {
        assert!(self.sessions().await.is_empty(), "no session is left");
        let worktrees = self.git(&["worktree", "list", "--porcelain"]);
        assert_eq!(
            worktrees
                .lines()
                .filter(|line| line.starts_with("worktree "))
                .count(),
            1,
            "only the main checkout is registered: {worktrees}"
        );
        let leftover: Vec<_> = std::fs::read_dir(&*self.worktree_root)
            .expect("read the worktree root")
            .collect();
        assert!(leftover.is_empty(), "no worktree directory: {leftover:?}");
        let bindings =
            std::fs::read_to_string(self.state.join("metadata.jsonl")).unwrap_or_default();
        assert!(
            !bindings.contains("\"worktree\""),
            "no worktree binding is left: {bindings}"
        );
        for ran in ["pre-remove", "post-remove"] {
            assert!(self.marker(ran).exists(), "{ran} ran during the rollback");
        }
        for never in ["agent-started", "session-start"] {
            assert!(!self.marker(never).exists(), "{never} never happened");
        }
        assert_ne!(
            self.git(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{BRANCH}")
            ]),
            "",
            "the created branch is kept"
        );
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = self.server.await;
    }
}

/// A script that dirties its worktree, writes to stderr and fails.
const FAILING_HOOK: &str = "#!/bin/sh\ntouch untracked-by-the-hook\n\
                            echo 'npm install failed' >&2\nexit 1\n";

#[tokio::test]
async fn a_failing_required_host_hook_aborts_the_create_and_skips_the_repository_script() {
    let mut rig = Rig::start("req-host", Settings::REQUIRED).await;
    rig.host_hook("post-create", FAILING_HOOK);
    let repo_ran = rig.marker("repo-script-ran");
    rig.committed_script(
        "hooks/post-create",
        &format!("#!/bin/sh\ntouch '{}'\n", repo_ran.display()),
    );

    let error = err_payload(rig.new_session().await);

    assert_eq!(error.code, "hook_required_failed", "{}", error.msg);
    assert_eq!(error.class, ErrorClass::Runtime);
    assert!(
        error
            .msg
            .contains("required post-create hook failed on the host layer")
            && error.msg.contains("exit status: 1"),
        "{}",
        error.msg
    );
    assert!(
        error.msg.contains("not run because of it:")
            && error.msg.contains(".pohunek/hooks/post-create"),
        "the skipped repository script is named: {}",
        error.msg
    );
    assert!(
        error.msg.ends_with("; stderr tail:\nnpm install failed"),
        "{}",
        error.msg
    );
    assert!(!repo_ran.exists(), "the repository script did not run");
    rig.assert_rolled_back().await;
    rig.stop().await;
}

#[tokio::test]
async fn a_required_hook_that_times_out_aborts_the_create() {
    let mut rig = Rig::start(
        "req-timeout",
        Settings {
            timeout: Some(SHORT_HOOK_TIMEOUT),
            ..Settings::REQUIRED
        },
    )
    .await;
    rig.host_hook("post-create", "#!/bin/sh\nexec sleep 30\n");

    let error = err_payload(rig.new_session().await);

    assert_eq!(error.code, "hook_required_failed", "{}", error.msg);
    assert!(
        error.msg.contains("host layer") && error.msg.contains("did not finish within"),
        "{}",
        error.msg
    );
    rig.assert_rolled_back().await;
    rig.stop().await;
}

#[tokio::test]
async fn a_failing_required_repository_hook_aborts_the_create() {
    let mut rig = Rig::start("req-repo", Settings::REQUIRED).await;
    rig.host_hook("post-create", "#!/bin/sh\nexit 0\n");
    rig.committed_script("hooks/post-create", FAILING_HOOK);

    let error = err_payload(rig.new_session().await);

    assert_eq!(error.code, "hook_required_failed", "{}", error.msg);
    assert!(
        error.msg.contains("repository layer") && !error.msg.contains("not run because of it"),
        "{}",
        error.msg
    );
    rig.assert_rolled_back().await;
    rig.stop().await;
}

#[tokio::test]
async fn a_failing_required_legacy_setup_script_aborts_the_create() {
    let mut rig = Rig::start("req-setup", Settings::REQUIRED).await;
    rig.committed_script("setup", FAILING_HOOK);

    let error = err_payload(rig.new_session().await);

    assert_eq!(error.code, "hook_required_failed", "{}", error.msg);
    assert!(error.msg.contains("repository layer"), "{}", error.msg);
    rig.assert_rolled_back().await;
    rig.stop().await;
}

#[tokio::test]
async fn a_required_failure_reports_no_stderr_when_the_capture_is_off() {
    let mut rig = Rig::start(
        "req-notail",
        Settings {
            stderr_tail_bytes: Some(0),
            ..Settings::REQUIRED
        },
    )
    .await;
    rig.host_hook("post-create", FAILING_HOOK);

    let error = err_payload(rig.new_session().await);

    assert_eq!(error.code, "hook_required_failed", "{}", error.msg);
    assert!(
        !error.msg.contains("stderr tail") && !error.msg.contains("npm install failed"),
        "{}",
        error.msg
    );
    rig.assert_rolled_back().await;
    rig.stop().await;
}

#[tokio::test]
async fn succeeding_required_hooks_and_a_missing_script_create_the_session() {
    let mut rig = Rig::start("req-ok", Settings::REQUIRED).await;
    // No script anywhere: nothing runs and nothing fails.
    let bare = ok_payload(rig.new_session().await);
    let bare: SessionInfo = serde_json::from_value(bare).expect("session info");
    assert!(bare.warnings.is_empty(), "{:?}", bare.warnings);

    rig.host_hook("post-create", "#!/bin/sh\nexit 0\n");
    rig.committed_script("hooks/post-create", "#!/bin/sh\nexit 0\n");
    let second = create_worktree_session(
        &mut rig.control,
        RuntimeRef::shell(),
        rig.repo.cwd().to_path_buf(),
        "c",
    )
    .await;
    let second: SessionInfo = serde_json::from_value(ok_payload(second)).expect("session info");
    assert!(second.warnings.is_empty(), "{:?}", second.warnings);
    rig.stop().await;
}

#[tokio::test]
async fn an_unrequired_failing_hook_still_warns() {
    let mut rig = Rig::start(
        "req-off",
        Settings {
            required: false,
            ..Settings::REQUIRED
        },
    )
    .await;
    rig.host_hook("post-create", FAILING_HOOK);

    let created: SessionInfo =
        serde_json::from_value(ok_payload(rig.new_session().await)).expect("session info");

    let [warning] = created.warnings.as_slice() else {
        panic!("one hook warning expected: {:?}", created.warnings);
    };
    assert_eq!(warning.kind, SessionWarningKind::Hook);
    rig.stop().await;
}

#[tokio::test]
async fn a_failing_pre_create_hook_stays_a_warning_and_never_lends_its_stderr_to_the_error() {
    let mut rig = Rig::start("req-pre", Settings::REQUIRED).await;
    rig.host_hook(
        "pre-create",
        "#!/bin/sh\necho 'pre-create noise' >&2\nexit 1\n",
    );

    // Only pre-create fails: the create succeeds with its warning.
    rig.host_hook("post-create", "#!/bin/sh\nexit 0\n");
    let created: SessionInfo =
        serde_json::from_value(ok_payload(rig.new_session().await)).expect("session info");
    let [warning] = created.warnings.as_slice() else {
        panic!("one hook warning expected: {:?}", created.warnings);
    };
    assert_eq!(warning.kind, SessionWarningKind::Hook);

    // Both fail: the error carries the post-create tail only.
    rig.host_hook("post-create", FAILING_HOOK);
    let error = err_payload(
        create_worktree_session(
            &mut rig.control,
            RuntimeRef::shell(),
            rig.repo.cwd().to_path_buf(),
            "c",
        )
        .await,
    );
    assert_eq!(error.code, "hook_required_failed", "{}", error.msg);
    assert!(
        error.msg.ends_with("; stderr tail:\nnpm install failed")
            && !error.msg.contains("pre-create noise"),
        "{}",
        error.msg
    );
    rig.stop().await;
}
