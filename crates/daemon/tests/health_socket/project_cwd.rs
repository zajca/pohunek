//! `session.new` with a project reference and an explicit `cwd`, through the
//! public control socket.
//!
//! A project-bound in-place session runs in the requested directory and keeps
//! the project association of the reference; a directory that cannot be used
//! is refused before any session exists.

// Rust guideline compliant 2026-10-22

use super::*;

/// Interval at which the daemon reads the cwd of a session's focus process,
/// short so a scenario observes a `cd` promptly.
const PROCWATCH_POLL: Duration = Duration::from_millis(50);

/// A started daemon with one registered project.
struct ProjectRig {
    control: Framed<UnixStream, LinesCodec>,
    shutdown: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
    repo: TestEnv,
    outside: TestDir,
    project: ProjectInfo,
    _socket: TestSocket,
    _state: TestDir,
}

impl ProjectRig {
    async fn start(tag: &str) -> Self {
        let repo = init_git_repo();
        let state = temp_dir(&format!("{tag}-state"));
        let socket = temp_socket(tag);
        let config = SessionRegistryConfig {
            shell_command: support::hermetic_shell(),
            store_path: Some(state.join("metadata.jsonl")),
            procwatch_poll: PROCWATCH_POLL,
            ..SessionRegistryConfig::default()
        };
        let (shutdown, server, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
        let mut control = connect(&socket).await;
        let add = Request::make(
            "project-cwd-add",
            method::PROJECT_ADD,
            serde_json::json!({ "path": repo.cwd() }),
        );
        let project: ProjectInfo =
            serde_json::from_value(ok_payload(exchange(&mut control, &add).await))
                .expect("project info");
        Self {
            control,
            shutdown,
            server,
            repo,
            outside: temp_dir(&format!("{tag}-outside")),
            project,
            _socket: socket,
            _state: state,
        }
    }

    /// Starts a shell session in the project, optionally in `cwd`.
    async fn new_session(&mut self, cwd: Option<PathBuf>) -> Response {
        let mut params = session_params_in(PathBuf::new());
        params.cwd = cwd;
        params.project = Some(self.project.id.clone());
        create_session_with_params(&mut self.control, params).await
    }

    /// Types `cd <dir>` into the session and returns its info once the daemon
    /// observed the new directory.
    async fn move_session_to(&mut self, id: &SessionId, dir: &Path) -> SessionInfo {
        input_session(&mut self.control, id, &format!("cd '{}'\n", dir.display())).await;
        let target = dir.canonicalize().expect("canonical target directory");
        let control = tokio::sync::Mutex::new(&mut self.control);
        wait_until("the daemon to observe the new directory", || async {
            let info = inspect_session(&mut *control.lock().await, id).await;
            (info.cwd == target).then_some(info)
        })
        .await
    }

    async fn session_count(&mut self) -> usize {
        let list = Request::make(
            "project-cwd-list",
            method::SESSION_LIST,
            serde_json::to_value(SessionListParams::default()).expect("list params"),
        );
        let sessions: Vec<SessionInfo> =
            serde_json::from_value(ok_payload(exchange(&mut self.control, &list).await))
                .expect("session list");
        sessions.len()
    }

    async fn finish(self) {
        let _ = self.shutdown.send(());
        let _ = self.server.await;
    }
}

/// Compares two paths of the same directory in canonical form, because the
/// daemon and the test may reach a temporary directory through different
/// symlinked prefixes (`/var` and `/private/var` on macOS).
fn assert_same_directory(actual: &Path, expected: &Path, what: &str) {
    assert_eq!(
        actual.canonicalize().expect("canonical session cwd"),
        expected.canonicalize().expect("canonical expected cwd"),
        "{what}"
    );
}

#[tokio::test]
async fn a_project_session_starts_in_a_directory_outside_the_checkout() {
    let mut rig = ProjectRig::start("pcwd-outside").await;
    let outside = rig.outside.to_path_buf();

    let session: SessionInfo =
        serde_json::from_value(ok_payload(rig.new_session(Some(outside.clone())).await))
            .expect("session info");

    assert_eq!(
        session.cwd, outside,
        "the session starts where it was asked"
    );
    assert_eq!(
        session.project_id.as_deref(),
        Some(rig.project.id.as_str()),
        "the project association stays the reference's"
    );
    rig.finish().await;
}

#[tokio::test]
async fn a_project_session_starts_in_a_directory_inside_the_checkout() {
    let mut rig = ProjectRig::start("pcwd-inside").await;
    let nested = rig.repo.cwd().join("nested");
    std::fs::create_dir(&nested).expect("create the nested directory");

    let session: SessionInfo =
        serde_json::from_value(ok_payload(rig.new_session(Some(nested.clone())).await))
            .expect("session info");

    assert_same_directory(&session.cwd, &nested, "the nested directory");
    assert_eq!(session.project_id.as_deref(), Some(rig.project.id.as_str()));
    rig.finish().await;
}

#[tokio::test]
async fn a_project_session_without_a_directory_starts_in_the_checkout() {
    let mut rig = ProjectRig::start("pcwd-default").await;

    let session: SessionInfo =
        serde_json::from_value(ok_payload(rig.new_session(None).await)).expect("session info");

    assert_same_directory(&session.cwd, &rig.project.repo_root, "the project checkout");
    rig.finish().await;
}

#[tokio::test]
async fn a_project_session_refuses_a_directory_it_cannot_use_and_creates_nothing() {
    let mut rig = ProjectRig::start("pcwd-refused").await;
    let missing = rig.outside.join("missing");
    let file = rig.outside.join("file");
    std::fs::write(&file, b"not a directory").expect("write the file");

    for (label, cwd) in [
        ("missing", missing),
        ("a file", file),
        ("relative", PathBuf::from("relative/dir")),
    ] {
        let error = err_payload(rig.new_session(Some(cwd.clone())).await);
        assert_eq!(error.code, "bad_request", "{label}: {error:?}");
        assert!(
            error.msg.contains(&cwd.display().to_string()),
            "{label}: the error names the directory: {error:?}"
        );
    }
    assert_eq!(
        rig.session_count().await,
        0,
        "no refused request left a session"
    );
    rig.finish().await;
}

#[tokio::test]
async fn the_host_advertises_that_it_honors_the_directory() {
    let mut rig = ProjectRig::start("pcwd-capability").await;
    let inspect = Request::make("project-cwd-inspect", method::HOST_INSPECT, Value::Null);

    let capabilities: protocol::HostCapabilities =
        serde_json::from_value(ok_payload(exchange(&mut rig.control, &inspect).await))
            .expect("host capabilities");

    assert!(capabilities.session_new_cwd_supported);
    rig.finish().await;
}

#[tokio::test]
async fn a_repo_session_starts_in_the_requested_directory() {
    let mut rig = ProjectRig::start("pcwd-repo").await;
    let outside = rig.outside.to_path_buf();
    let mut params = session_params_in(outside.clone());
    params.repo = Some(rig.repo.cwd().to_path_buf());

    let session: SessionInfo = serde_json::from_value(ok_payload(
        create_session_with_params(&mut rig.control, params).await,
    ))
    .expect("session info");

    assert_same_directory(&session.cwd, &outside, "the --repo session's directory");
    assert_eq!(session.project_id.as_deref(), Some(rig.project.id.as_str()));
    rig.finish().await;
}

#[tokio::test]
async fn a_repo_session_refuses_a_missing_directory() {
    let mut rig = ProjectRig::start("pcwd-repo-missing").await;
    let missing = rig.outside.join("missing");
    let mut params = session_params_in(missing);
    params.repo = Some(rig.repo.cwd().to_path_buf());

    let error = err_payload(create_session_with_params(&mut rig.control, params).await);

    assert_eq!(error.code, "bad_request", "{error:?}");
    assert_eq!(rig.session_count().await, 0);
    rig.finish().await;
}

/// Without a project reference the project is detected from the directory, so a
/// subdirectory of a checkout probes where such a session starts.
#[tokio::test]
async fn a_detected_project_session_starts_in_the_requested_subdirectory() {
    let mut rig = ProjectRig::start("pcwd-detected").await;
    let nested = rig.repo.cwd().join("nested");
    std::fs::create_dir(&nested).expect("create the nested directory");

    let session: SessionInfo = serde_json::from_value(ok_payload(
        create_session_with_params(&mut rig.control, session_params_in(nested.clone())).await,
    ))
    .expect("session info");

    assert_same_directory(&session.cwd, &nested, "the requested subdirectory");
    rig.finish().await;
}

#[tokio::test]
async fn a_project_session_keeps_its_project_when_it_moves_outside_every_repository() {
    let mut rig = ProjectRig::start("pcwd-keeps").await;
    let first = rig.outside.to_path_buf();
    let second = rig.outside.join("second");
    std::fs::create_dir(&second).expect("create the second directory");
    let session: SessionInfo =
        serde_json::from_value(ok_payload(rig.new_session(Some(first)).await))
            .expect("session info");

    let moved = rig.move_session_to(&session.id, &second).await;

    assert_eq!(
        moved.project_id.as_deref(),
        Some(rig.project.id.as_str()),
        "an observed directory outside every repository keeps the reference's project"
    );
    rig.finish().await;
}

#[tokio::test]
async fn a_project_session_follows_a_directory_inside_another_repository() {
    let mut rig = ProjectRig::start("pcwd-follows").await;
    let other_repo = init_git_repo();
    let session: SessionInfo = serde_json::from_value(ok_payload(
        rig.new_session(Some(rig.outside.to_path_buf())).await,
    ))
    .expect("session info");

    let moved = rig.move_session_to(&session.id, other_repo.cwd()).await;

    assert_ne!(
        moved.project_id.as_deref(),
        Some(rig.project.id.as_str()),
        "a directory inside another repository changes the association"
    );
    assert!(moved.project_id.is_some(), "{moved:?}");
    rig.finish().await;
}

#[tokio::test]
async fn a_symlinked_launch_directory_is_stored_in_canonical_form() {
    let mut rig = ProjectRig::start("pcwd-symlink").await;
    let real = rig.outside.join("real");
    let link = rig.outside.join("link");
    std::fs::create_dir(&real).expect("create the real directory");
    std::os::unix::fs::symlink(&real, &link).expect("create the symlink");

    let session: SessionInfo =
        serde_json::from_value(ok_payload(rig.new_session(Some(link)).await))
            .expect("session info");

    assert_eq!(
        session.cwd,
        real.canonicalize().expect("canonical real directory"),
        "the stored launch directory is the path the process tree reports"
    );
    rig.finish().await;
}
