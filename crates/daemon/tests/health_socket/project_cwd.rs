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
            let mut guard = control.lock().await;
            let info = inspect_session(&mut guard, id).await;
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
async fn a_symlinked_launch_directory_is_stored_and_observed_in_canonical_form() {
    let mut rig = ProjectRig::start("pcwd-symlink").await;
    let real = rig.outside.join("real");
    let nested = real.join("nested");
    let link = rig.outside.join("link");
    std::fs::create_dir_all(&nested).expect("create the real directories");
    std::os::unix::fs::symlink(&real, &link).expect("create the symlink");

    let session: SessionInfo =
        serde_json::from_value(ok_payload(rig.new_session(Some(link.clone())).await))
            .expect("session info");
    assert_eq!(
        session.cwd,
        real.canonicalize().expect("canonical real directory"),
        "the stored launch directory is the path the process tree reports"
    );

    let moved = rig.move_session_to(&session.id, &link.join("nested")).await;

    assert_eq!(
        moved.cwd,
        nested.canonicalize().expect("canonical nested directory"),
        "the observation reports the canonical path"
    );
    assert_eq!(
        moved.project_id.as_deref(),
        Some(rig.project.id.as_str()),
        "the reference's project survives the observation of the canonical path"
    );
    rig.finish().await;
}

#[tokio::test]
async fn a_project_session_launched_in_the_checkout_follows_its_directory_out() {
    let mut rig = ProjectRig::start("pcwd-clears").await;
    let session: SessionInfo =
        serde_json::from_value(ok_payload(rig.new_session(None).await)).expect("session info");
    assert_eq!(session.project_id.as_deref(), Some(rig.project.id.as_str()));

    let outside = rig.outside.to_path_buf();
    let moved = rig.move_session_to(&session.id, &outside).await;

    assert_eq!(
        moved.project_id, None,
        "a session launched inside a repository follows its directory out of the project"
    );
    rig.finish().await;
}

/// K3: the reference association comes back when a session that followed
/// another repository returns outside every repository.
#[tokio::test]
async fn a_project_session_that_entered_a_repository_gets_its_reference_back_outside() {
    let mut rig = ProjectRig::start("pcwd-reenter").await;
    let other_repo = init_git_repo();
    let outside = rig.outside.to_path_buf();
    let session: SessionInfo =
        serde_json::from_value(ok_payload(rig.new_session(Some(outside.clone())).await))
            .expect("session info");
    let inside = rig.move_session_to(&session.id, other_repo.cwd()).await;
    assert_ne!(
        inside.project_id.as_deref(),
        Some(rig.project.id.as_str()),
        "inside another repository the session follows it"
    );

    let back = rig.move_session_to(&session.id, &outside).await;

    assert_eq!(
        back.project_id.as_deref(),
        Some(rig.project.id.as_str()),
        "back outside every repository the reference's project returns"
    );
    rig.finish().await;
}

/// K4: a directory whose repository state cannot be read is not evidence of
/// being outside every repository.
#[tokio::test]
async fn a_project_session_does_not_keep_its_project_on_unreadable_repository_evidence() {
    let mut rig = ProjectRig::start("pcwd-unreadable").await;
    let broken = rig.outside.join("broken");
    std::fs::create_dir(&broken).expect("create the broken checkout");
    std::fs::write(broken.join(".git"), "gitdir: /nonexistent/pohunek-gitdir\n")
        .expect("write the dangling .git file");
    let session: SessionInfo = serde_json::from_value(ok_payload(
        rig.new_session(Some(rig.outside.to_path_buf())).await,
    ))
    .expect("session info");

    let moved = rig.move_session_to(&session.id, &broken).await;

    assert_eq!(
        moved.project_id, None,
        "a failed detection never keeps the reference's project"
    );
    rig.finish().await;
}

/// K4: when `git` cannot answer, the walk alone is not evidence of being
/// outside every repository. `PATH` is narrowed only after the session exists.
#[tokio::test]
async fn a_project_session_does_not_keep_its_project_when_git_is_unavailable() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let mut rig = ProjectRig::start("pcwd-no-git").await;
    let second = rig.outside.join("second");
    std::fs::create_dir(&second).expect("create the second directory");
    let session: SessionInfo = serde_json::from_value(ok_payload(
        rig.new_session(Some(rig.outside.to_path_buf())).await,
    ))
    .expect("session info");
    let no_git = temp_dir("pcwd-no-git-path");
    let mut environment = pohunek_test_support::process_env::ProcessEnv::lock();
    environment.set("PATH", &*no_git);

    let moved = rig.move_session_to(&session.id, &second).await;
    drop(environment);

    assert_eq!(
        moved.project_id, None,
        "a git that cannot answer never keeps the reference's project"
    );
    rig.finish().await;
}

/// The first `PATH` entry holding an executable `git`.
fn real_git() -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH is set");
    std::env::split_paths(&path)
        .map(|directory| directory.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("git on PATH")
}

/// K5: a session without a launch marker never pays for the confirmation that
/// a directory lies outside every repository, which only the marker consumes.
/// The confirmation is the one `git` call that runs under `LC_ALL=C`, so the
/// daemon's own locale is set to something else and a recording `git` in front
/// of the real one tells the calls apart.
#[tokio::test]
async fn a_session_without_a_launch_marker_runs_no_outside_confirmation() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let mut rig = ProjectRig::start("pcwd-no-confirm").await;
    let alpha = rig.outside.join("alpha-dir");
    let beta = rig.outside.join("beta-dir");
    std::fs::create_dir(&alpha).expect("create the unmarked session's target");
    std::fs::create_dir(&beta).expect("create the marked session's target");
    let unmarked: SessionInfo =
        serde_json::from_value(ok_payload(rig.new_session(None).await)).expect("session info");
    let marked: SessionInfo = serde_json::from_value(ok_payload(
        rig.new_session(Some(rig.outside.to_path_buf())).await,
    ))
    .expect("session info");

    let mut environment = ProcessEnv::lock();
    let real = real_git();
    let recorder = temp_dir("pcwd-no-confirm-git");
    let log = recorder.join("git.log");
    write_executable(
        &recorder.join("git"),
        &format!(
            "#!/bin/sh\nprintf '%s %s\\n' \"${{LC_ALL-unset}}\" \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            real.display()
        ),
    );
    let path = std::env::var_os("PATH").expect("PATH is set");
    let mut entries = vec![recorder.to_path_buf()];
    entries.extend(std::env::split_paths(&path));
    environment.set("PATH", std::env::join_paths(entries).expect("join PATH"));
    environment.set("LC_ALL", "pohunek-test-locale");

    rig.move_session_to(&unmarked.id, &alpha).await;
    rig.move_session_to(&marked.id, &beta).await;
    drop(environment);

    let calls = std::fs::read_to_string(&log).expect("the recording git ran");
    let confirmations = |directory: &str| {
        calls
            .lines()
            .filter(|line| line.starts_with("C ") && line.contains(directory))
            .count()
    };
    assert!(
        confirmations("beta-dir") > 0,
        "the marked session's directory is confirmed: {calls}"
    );
    assert_eq!(
        confirmations("alpha-dir"),
        0,
        "no session marker, no confirmation: {calls}"
    );
    rig.finish().await;
}
