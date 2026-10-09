//! Claude recovery through the public control socket and a real worker.

// Rust guideline compliant 2026-10-09

use super::*;

const LAUNCH: &str = "fixture-claude-launch";
const CLEAR: &str = "fixture-claude-clear";
const SELECTED: &str = "fixture-claude-selected";

async fn listed_sessions(control: &mut Framed<UnixStream, LinesCodec>) -> Vec<SessionInfo> {
    serde_json::from_value(ok_payload(
        exchange(
            control,
            &Request::make("list-batched-activity", method::SESSION_LIST, Value::Null),
        )
        .await,
    ))
    .expect("public session list")
}

fn activity_for<'a>(id: &SessionId, sessions: &'a [SessionInfo]) -> Option<&'a str> {
    sessions
        .iter()
        .find(|session| &session.id == id)
        .expect("session appears in list")
        .native_last_activity_at
        .as_deref()
}

struct ClaudeRig {
    socket: TestSocket,
    control: Framed<UnixStream, LinesCodec>,
    shutdown: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
    session: SessionInfo,
    argv_log: PathBuf,
    ack_log: PathBuf,
    config_home: PathBuf,
    worker_state_root: PathBuf,
    store_path: PathBuf,
    additional_sessions: Vec<SessionId>,
    _bin: TestDir,
    _state: TestDir,
    cwd: TestDir,
    _agents: TestDir,
    _config: TestDir,
}

impl ClaudeRig {
    fn journal_path(&self) -> PathBuf {
        let session_dir = self.worker_state_root.join(self.session.id.0.as_str());
        let mut journals = std::fs::read_dir(&session_dir)
            .expect("read the session's worker journal directory")
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|name| name == "json"))
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        journals.sort();
        assert_eq!(journals.len(), 1, "one worker generation: {journals:?}");
        journals.remove(0)
    }

    fn rewrite_durable_worker_id(&self, expected: &str, replacement: &str) {
        let original = std::fs::read(&self.store_path).expect("read durable session store");
        let mut changed = false;
        let lines = original
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                let mut record: serde_json::Value =
                    serde_json::from_slice(line).expect("durable record decodes");
                if record["kind"] == "session"
                    && record["session_id"] == self.session.id.0.as_str()
                    && record["runtime"]["worker_id"] == expected
                {
                    record["runtime"]["worker_id"] = serde_json::json!(replacement);
                    changed = true;
                }
                serde_json::to_vec(&record).expect("durable record serializes")
            })
            .collect::<Vec<_>>();
        assert!(changed, "the durable record names a worker to mismatch");
        let mut rewritten = lines.join(&b'\n');
        rewritten.push(b'\n');
        std::fs::write(&self.store_path, rewritten).expect("rewrite durable session store");
    }

    async fn new(tag: &str) -> Self {
        let bin = temp_dir(&format!("{tag}-bin"));
        let state = temp_dir(&format!("{tag}-state"));
        let cwd = temp_dir(&format!("{tag}-cwd"));
        let agents = temp_dir(&format!("{tag}-agents"));
        let config = temp_dir(&format!("{tag}-config"));
        let config_home = config.join(".claude");
        std::fs::create_dir(&config_home).expect("create fixture Claude home");
        pohunek_daemon::integration::install_claude(&config_home)
            .expect("install managed Claude hook");
        let hook = config_home.join("hooks/pohunek-agent-state.sh");
        let argv_log = state.join("argv.log");
        let ack_log = state.join("hook-ack.log");
        let script = bin.join("fixture-agent.sh");
        write_executable(
            &script,
            &format!(
                "#!/bin/bash\n\
                 printf '%s\\n' \"$*\" >> {argv:?}\n\
                 printf '\\033[?2004h'\n\
                 while read -r action source reference; do\n\
                   case \"$action\" in\n\
                     report) printf '{{\"session_id\":\"%s\",\"source\":\"%s\"}}' \"$reference\" \"$source\" | /bin/sh {hook:?} session; printf '%s\\n' \"$reference\" >> {ack:?} ;;\n\
                   esac\n\
                 done\n",
                argv = argv_log.display().to_string(),
                hook = hook.display().to_string(),
                ack = ack_log.display().to_string(),
            ),
        );
        let program = bin.join("claude");
        // hermetic-allowed: #421 copy the host Bash image into a private fake Claude fixture.
        std::fs::copy("/bin/bash", &program).expect("copy fixture agent executable");
        std::fs::write(
            agents.join("claude-fixture.toml"),
            format!(
                "base = \"claude\"\nprogram = \"{}\"\nargs = [\"{}\"]\n[env]\nCLAUDE_CONFIG_DIR = \"{}\"\n",
                program.display(),
                script.display(),
                config_home.display(),
            ),
        )
        .expect("write Claude host profile");
        let socket = temp_socket(tag);
        let store_path = state.join("metadata.jsonl");
        let registry_config = SessionRegistryConfig {
            shell_command: support::hermetic_shell(),
            store_path: Some(store_path.clone()),
            agents_dir: Some(agents.to_path_buf()),
            host_state_dir: Some(state.join("host-state")),
            ..SessionRegistryConfig::default()
        };
        let (shutdown, server, worker_home) =
            spawn_server_with_config(&socket, "0.0.0", registry_config).await;
        let worker_state_root = worker_home.join("state/pohunek/workers");
        let mut control = connect(&socket).await;
        let mut params = session_params_in(cwd.to_path_buf());
        "claude-fixture".clone_into(&mut params.agent);
        let session: SessionInfo = serde_json::from_value(ok_payload(
            create_session_with_params(&mut control, params).await,
        ))
        .expect("create fixture Claude session");
        wait_until("fixture agent to start reading its PTY", || async {
            argv_log.exists().then_some(())
        })
        .await;
        Self {
            socket,
            control,
            shutdown,
            server,
            session,
            argv_log,
            ack_log,
            config_home,
            worker_state_root,
            store_path,
            additional_sessions: Vec::new(),
            _bin: bin,
            _state: state,
            cwd,
            _agents: agents,
            _config: config,
        }
    }

    async fn report(&mut self, source: &str, reference: &str) {
        let submitted = input_session(
            &mut self.control,
            &self.session.id,
            &format!("report {source} {reference}"),
        )
        .await;
        assert!(submitted.accepted, "fixture hook command accepted");
        read_file_until(&self.ack_log, reference.as_bytes()).await;
        let control = tokio::sync::Mutex::new(&mut self.control);
        wait_until("Claude hook report to reach session.inspect", || async {
            let mut control = control.lock().await;
            let session = inspect_session(&mut control, &self.session.id).await;
            (session.active_agent_session_id.as_deref() == Some(reference)).then_some(())
        })
        .await;
    }

    fn transcript(&self, reference: &str) {
        let project = self.config_home.join("projects/fixture");
        std::fs::create_dir_all(&project).expect("create fixture transcript directory");
        std::fs::write(project.join(format!("{reference}.jsonl")), "{}\n")
            .expect("write fixture transcript");
    }

    async fn start_with_reference(&mut self, reference: &str) -> SessionId {
        let mut params = session_params_in(self.cwd.to_path_buf());
        "claude-fixture".clone_into(&mut params.agent);
        let session: SessionInfo = serde_json::from_value(ok_payload(
            create_session_with_params(&mut self.control, params).await,
        ))
        .expect("create another Claude session");
        let id = session.id;
        self.additional_sessions.push(id.clone());
        let submitted = input_session(
            &mut self.control,
            &id,
            &format!("report startup {reference}"),
        )
        .await;
        assert!(submitted.accepted, "fixture hook command accepted");
        let control = tokio::sync::Mutex::new(&mut self.control);
        wait_until(
            "another Claude hook report to reach session.inspect",
            || async {
                let mut control = control.lock().await;
                let session = inspect_session(&mut control, &id).await;
                (session.active_agent_session_id.as_deref() == Some(reference)).then_some(())
            },
        )
        .await;
        id
    }

    async fn stop(&mut self, id: &SessionId) -> SessionInfo {
        let request = Request::make(
            "claude-recovery-stop",
            method::SESSION_STOP,
            serde_json::to_value(id).expect("serialize session id"),
        );
        ok_payload(exchange(&mut self.control, &request).await);
        inspect_session(&mut self.control, id).await
    }

    async fn resume(&mut self) -> Response {
        let request = Request::make(
            "claude-recovery-resume",
            method::SESSION_RESUME,
            serde_json::to_value(&self.session.id).expect("serialize session id"),
        );
        exchange(&mut self.control, &request).await
    }

    async fn finish(mut self, child: Option<SessionId>) {
        for id in child
            .into_iter()
            .chain(self.additional_sessions.clone())
            .chain(std::iter::once(self.session.id.clone()))
        {
            let _ = self.stop(&id).await;
            let request = Request::make(
                "claude-recovery-remove",
                method::SESSION_REMOVE,
                serde_json::to_value(&id).expect("serialize session id"),
            );
            ok_payload(exchange(&mut self.control, &request).await);
        }
        let _ = self.shutdown.send(());
        self.server.await.expect("control server stops");
        let _ = self.socket;
    }
}

async fn assert_conversation_switch(source: &str, selected: &str) {
    let mut rig = ClaudeRig::new(&format!("claude-{source}-recovery")).await;
    rig.transcript(LAUNCH);
    rig.transcript(selected);
    rig.report("startup", LAUNCH).await;
    rig.report(source, selected).await;
    let id = rig.session.id.clone();
    rig.stop(&id).await;
    let resumed = rig.resume().await;
    let argv = read_file_until(&rig.argv_log, b"--resume").await;
    rig.finish(None).await;

    assert!(resumed.is_ok(), "verified Claude conversation is resumable");
    assert!(
        String::from_utf8_lossy(&argv).contains(&format!("--resume {selected}")),
        "recovery must launch the selected Claude conversation; argv log: {}",
        String::from_utf8_lossy(&argv)
    );
}

#[tokio::test]
async fn claude_clear_recovery_selects_the_verified_new_conversation() {
    assert_conversation_switch("clear", CLEAR).await;
}

#[tokio::test]
async fn claude_in_session_resume_selects_the_verified_conversation() {
    assert_conversation_switch("resume", SELECTED).await;
}

#[tokio::test]
async fn claude_public_inspect_and_list_show_verified_transcript_activity() {
    let mut rig = ClaudeRig::new("claude-public-activity").await;
    rig.transcript(LAUNCH);
    rig.transcript(CLEAR);
    let transcript = rig
        .config_home
        .join("projects/fixture")
        .join(format!("{CLEAR}.jsonl"));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&transcript)
        .expect("open selected transcript");
    file.set_times(
        std::fs::FileTimes::new()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)),
    )
    .expect("set fixture transcript activity");
    rig.report("startup", LAUNCH).await;
    rig.report("clear", CLEAR).await;

    let inspected = inspect_session(&mut rig.control, &rig.session.id).await;
    let listed: Vec<SessionInfo> = serde_json::from_value(ok_payload(
        exchange(
            &mut rig.control,
            &Request::make("list-native-activity", method::SESSION_LIST, Value::Null),
        )
        .await,
    ))
    .expect("public session list");
    let listed = listed
        .into_iter()
        .find(|session| session.id == rig.session.id)
        .expect("listed Claude session");
    assert_eq!(inspected.native_session_id.as_deref(), Some(CLEAR));
    assert_eq!(listed.native_session_id.as_deref(), Some(CLEAR));
    assert_eq!(
        inspected.native_last_activity_at.as_deref(),
        Some("2023-11-14T22:13:20Z")
    );
    assert_eq!(
        listed.native_last_activity_at,
        inspected.native_last_activity_at
    );
    assert_ne!(
        inspected.native_last_activity_at.as_deref(),
        Some(inspected.updated_at.as_str())
    );

    std::fs::remove_file(transcript).expect("remove selected transcript");
    let missing = inspect_session(&mut rig.control, &rig.session.id).await;
    let listed_missing: Vec<SessionInfo> = serde_json::from_value(ok_payload(
        exchange(
            &mut rig.control,
            &Request::make("list-missing-activity", method::SESSION_LIST, Value::Null),
        )
        .await,
    ))
    .expect("public session list after removal");
    rig.finish(None).await;
    assert_eq!(missing.native_session_id.as_deref(), Some(CLEAR));
    assert_eq!(missing.native_last_activity_at, None);
    assert_eq!(listed_missing[0].native_last_activity_at, None);
}

#[tokio::test]
async fn claude_list_reads_multiple_targets_in_one_large_store_without_following_symlinks() {
    const FILLER_FILES: usize = 1_024;
    let mut rig = ClaudeRig::new("claude-batched-activity").await;
    let project = rig.config_home.join("projects/fixture");
    std::fs::create_dir_all(&project).expect("create fixture transcript directory");
    for index in 0..FILLER_FILES {
        std::fs::write(project.join(format!("unrelated-{index}.jsonl")), "{}\n")
            .expect("populate shared transcript store");
    }
    rig.transcript(LAUNCH);
    rig.transcript(CLEAR);
    for (reference, seconds) in [(LAUNCH, 1_700_000_000), (CLEAR, 1_700_000_100)] {
        std::fs::File::options()
            .write(true)
            .open(project.join(format!("{reference}.jsonl")))
            .expect("open transcript to set activity")
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds)),
            )
            .expect("set transcript activity");
    }
    let outside = rig.config_home.join("outside-conversation.jsonl");
    std::fs::write(&outside, "{}\n").expect("write outside transcript");
    std::os::unix::fs::symlink(&outside, project.join(format!("{SELECTED}.jsonl")))
        .expect("link transcript outside the store");
    rig.report("startup", LAUNCH).await;
    let clear_id = rig.start_with_reference(CLEAR).await;
    let symlink_id = rig.start_with_reference(SELECTED).await;
    let listed = listed_sessions(&mut rig.control).await;
    assert_eq!(
        activity_for(&rig.session.id, &listed),
        Some("2023-11-14T22:13:20Z")
    );
    assert_eq!(
        activity_for(&clear_id, &listed),
        Some("2023-11-14T22:15:00Z")
    );
    assert_eq!(activity_for(&symlink_id, &listed), None);

    std::fs::remove_file(project.join(format!("{CLEAR}.jsonl")))
        .expect("remove one transcript after the first list");
    let listed_again = listed_sessions(&mut rig.control).await;
    assert_eq!(
        activity_for(&rig.session.id, &listed_again),
        Some("2023-11-14T22:13:20Z")
    );
    assert_eq!(activity_for(&clear_id, &listed_again), None);
    assert_eq!(activity_for(&symlink_id, &listed_again), None);
    rig.finish(None).await;
}

fn pi_descriptor(program: &std::path::Path) -> String {
    format!(
        "schema = 1\nid = \"acme.runtime.pi\"\nversion = \"1.0.0\"\nruntime_api = 1\n\
         [runtime]\nid = \"pi\"\nname = \"Pi fixture\"\nprogram = {program:?}\nargs = []\n\
         detect_manifest = \"detect.toml\"\nprompt_arg = true\n\
         [input]\nbracketed_paste = false\nsubmit_delay_ms = 0\ntext_policy = \"unrestricted\"\n\
         [resume]\nsupported = true\nreference_kind = \"id\"\nargs = [\"--session\", \"{{reference}}\"]\n\
         [fork]\nsupported = true\nargs = [\"--fork\", \"{{reference}}\"]\n\
         [native_reference]\nstrategy = \"assigned\"\nlaunch_args = [\"--session-id\", \"{{reference}}\"]\n\
         [native_reference.existence]\ncheck = \"file\"\nroot_env = \"PI_CODING_AGENT_DIR\"\n\
         dir = \"sessions\"\nfile_name = \"_{{reference}}.jsonl\"\nname_match = \"ends_with\"\nmax_depth = 1\n",
        program = program.display().to_string(),
    )
}

fn pi_archive_from_descriptor(descriptor: String) -> (Vec<u8>, package::PackageDigest) {
    let archive = package::build_archive(
        &[
            package::ArchiveEntry {
                path: "runtime.toml".to_owned(),
                contents: descriptor.into_bytes(),
                executable: false,
            },
            package::ArchiveEntry {
                path: "detect.toml".to_owned(),
                contents: b"[[rules]]\nid = \"ready\"\nstate = \"idle\"\npriority = 100\nregion = \"whole_recent\"\nany = [{ contains = \"ready\" }]\n".to_vec(),
                executable: false,
            },
        ],
        &package::Limits::DEFAULT,
    )
    .expect("build fixture Pi package");
    let digest = package::read_archive(&archive, &package::Limits::DEFAULT)
        .expect("read fixture Pi package")
        .digest()
        .clone();
    (archive, digest)
}

fn pi_archive(program: &std::path::Path) -> (Vec<u8>, package::PackageDigest) {
    pi_archive_from_descriptor(pi_descriptor(program))
}

async fn install_pi_archive(
    control: &mut Framed<UnixStream, LinesCodec>,
    archive_path: &std::path::Path,
    digest: &package::PackageDigest,
) {
    let installed = exchange(
        control,
        &Request::make(
            "install-pi-batch-package",
            method::PACKAGE_INSTALL,
            serde_json::json!({
                "archive_path": archive_path,
                "trust": { "kind": "explicit_digest", "digest": digest },
                "enable": true,
                "select": true,
                "dry_run": false
            }),
        ),
    )
    .await;
    ok_payload(installed);
}

async fn assert_invalid_pi_descriptors_are_refused(
    control: &mut Framed<UnixStream, LinesCodec>,
    state: &std::path::Path,
    program: &std::path::Path,
) {
    let baseline: protocol::PackageListResult = serde_json::from_value(ok_payload(
        exchange(
            control,
            &Request::make("list-before-invalid-pi", method::PACKAGE_LIST, Value::Null),
        )
        .await,
    ))
    .expect("list installed package");
    let valid = pi_descriptor(program);
    let path_hook = valid
        .replace("reference_kind = \"id\"", "reference_kind = \"path\"")
        .replace(
            "strategy = \"assigned\"\nlaunch_args = [\"--session-id\", \"{reference}\"]",
            "strategy = \"hook\"",
        );
    assert_ne!(path_hook, valid, "path-kind hook descriptor differs");
    assert!(path_hook.contains("strategy = \"hook\""));
    assert!(!path_hook.contains("launch_args"));
    let deep_scan = valid.replace("max_depth = 1", "max_depth = 9");
    assert_ne!(deep_scan, valid, "invalid scan depth differs");
    for (name, descriptor) in [("path-hook", path_hook), ("deep-scan", deep_scan)] {
        let (archive, digest) = pi_archive_from_descriptor(descriptor);
        let archive_path = state.join(format!("invalid-{name}.tar.zst"));
        std::fs::write(&archive_path, archive).expect("write invalid fixture package");
        let response = exchange(
            control,
            &Request::make(
                name,
                method::PACKAGE_INSTALL,
                serde_json::json!({
                    "archive_path": archive_path,
                    "trust": { "kind": "explicit_digest", "digest": digest },
                    "enable": true,
                    "select": true,
                    "dry_run": false
                }),
            ),
        )
        .await;
        let Err(error) = response.into_result() else {
            panic!("invalid {name} descriptor must be rejected");
        };
        assert_eq!(error.code, "package_descriptor_invalid", "{name}");
    }
    let after: protocol::PackageListResult = serde_json::from_value(ok_payload(
        exchange(
            control,
            &Request::make("list-after-invalid-pi", method::PACKAGE_LIST, Value::Null),
        )
        .await,
    ))
    .expect("list packages after refused installs");
    assert_eq!(
        after, baseline,
        "refused installs cannot mutate the registry"
    );
}

fn write_pi_transcripts(config_home: &std::path::Path, sessions: &[SessionInfo]) -> PathBuf {
    let project = config_home.join("sessions/project");
    std::fs::create_dir_all(&project).expect("create Pi session store");
    for (index, seconds) in [(0, 1_700_000_000), (1, 1_700_000_100)] {
        let reference = sessions[index]
            .native_session_id
            .as_deref()
            .expect("Pi assigned ID");
        let transcript = project.join(format!("2026-10-10T0{index}_{reference}.jsonl"));
        std::fs::write(&transcript, "{}\n").expect("write Pi transcript");
        std::fs::File::options()
            .write(true)
            .open(&transcript)
            .expect("open Pi transcript")
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds)),
            )
            .expect("set Pi transcript activity");
    }
    let outside = config_home.join("outside.jsonl");
    std::fs::write(&outside, "{}\n").expect("write outside transcript");
    std::os::unix::fs::symlink(
        &outside,
        project.join(format!(
            "2026-10-10T02_{}.jsonl",
            sessions[2]
                .native_session_id
                .as_deref()
                .expect("Pi assigned ID")
        )),
    )
    .expect("link outside transcript");
    project
}

#[tokio::test]
async fn pi_package_list_batches_suffix_matches_and_ignores_symlinks() {
    let bin = temp_dir("pi-batch-bin");
    let state = temp_dir("pi-batch-state");
    let cwd = temp_dir("pi-batch-cwd");
    let agents = temp_dir("pi-batch-agents");
    let config = temp_dir("pi-batch-config");
    let program = bin.join("pi-fixture.sh");
    write_executable(
        &program,
        "#!/bin/sh\nprintf '\\033[?2004h'\nwhile IFS= read -r line; do :; done\n",
    );
    let (archive, digest) = pi_archive(&program);
    let archive_path = state.join("pi.tar.zst");
    std::fs::write(&archive_path, archive).expect("write fixture Pi archive");
    let config_home = config.join("pi");
    std::fs::create_dir(&config_home).expect("create Pi config home");
    std::fs::write(
        agents.join("pi-fixture.toml"),
        format!(
            "base = \"pi\"\npackage = \"acme.runtime.pi\"\ndigest = \"{digest}\"\n\
             [env]\nPI_CODING_AGENT_DIR = \"{}\"\n",
            config_home.display()
        ),
    )
    .expect("write pinned Pi profile");
    let socket = temp_socket("pi-batch-activity");
    let registry_config = SessionRegistryConfig {
        shell_command: support::hermetic_shell(),
        store_path: Some(state.join("metadata.jsonl")),
        agents_dir: Some(agents.to_path_buf()),
        plugins_dir: Some(state.join("plugins")),
        host_state_dir: Some(state.join("host-state")),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, server, _worker_home) =
        spawn_server_with_config(&socket, "0.0.0", registry_config).await;
    let mut control = connect(&socket).await;
    install_pi_archive(&mut control, &archive_path, &digest).await;
    assert_invalid_pi_descriptors_are_refused(&mut control, &state, &program).await;
    let mut sessions = Vec::new();
    for _ in 0..3 {
        let mut params = session_params_in(cwd.to_path_buf());
        "pi-fixture".clone_into(&mut params.agent);
        let session: SessionInfo = serde_json::from_value(ok_payload(
            create_session_with_params(&mut control, params).await,
        ))
        .expect("create package-backed Pi session");
        assert!(
            session.native_session_id.is_some(),
            "Pi gets an assigned ID"
        );
        sessions.push(session);
    }
    let project = write_pi_transcripts(&config_home, &sessions);
    let listed = listed_sessions(&mut control).await;
    let inspected = inspect_session(&mut control, &sessions[0].id).await;
    let absent = sessions[1]
        .native_session_id
        .as_deref()
        .expect("Pi assigned ID");
    std::fs::remove_file(project.join(format!("2026-10-10T01_{absent}.jsonl")))
        .expect("remove Pi transcript");
    let listed_missing = listed_sessions(&mut control).await;
    for session in &sessions {
        let request = Request::make(
            "remove-pi-batch-session",
            method::SESSION_REMOVE,
            serde_json::to_value(&session.id).expect("serialize session id"),
        );
        ok_payload(exchange(&mut control, &request).await);
    }
    let _ = shutdown.send(());
    server.await.expect("Pi control server stops");

    assert_eq!(
        activity_for(&sessions[0].id, &listed),
        Some("2023-11-14T22:13:20Z")
    );
    assert_eq!(
        activity_for(&sessions[1].id, &listed),
        Some("2023-11-14T22:15:00Z")
    );
    assert_eq!(activity_for(&sessions[2].id, &listed), None);
    assert_eq!(
        inspected.native_last_activity_at,
        activity_for(&sessions[0].id, &listed).map(str::to_owned)
    );
    assert_eq!(activity_for(&sessions[1].id, &listed_missing), None);
}

#[tokio::test]
async fn claude_missing_transcript_refuses_before_a_new_worker_generation() {
    let mut rig = ClaudeRig::new("claude-missing-transcript").await;
    rig.report("startup", LAUNCH).await;
    let id = rig.session.id.clone();
    let stopped = rig.stop(&id).await;
    let argv_before = std::fs::read(&rig.argv_log).expect("initial launch argv");
    let response = rig.resume().await;
    let after = inspect_session(&mut rig.control, &id).await;
    let argv_after = std::fs::read(&rig.argv_log).expect("argv after recovery attempt");
    rig.finish(None).await;

    let Err(error) = response.into_result() else {
        panic!("missing transcript must refuse recovery");
    };
    assert_eq!(error.code, "agent_native_reference_missing");
    assert_eq!(after.runtime, stopped.runtime);
    assert_eq!(argv_after, argv_before, "missing transcript cannot launch");
}

#[tokio::test]
async fn claude_missing_transcript_refuses_fork_before_creating_a_child() {
    let mut rig = ClaudeRig::new("claude-missing-fork-transcript").await;
    rig.report("startup", LAUNCH).await;
    let argv_before = std::fs::read(&rig.argv_log).expect("initial launch argv");
    let request = Request::make(
        "claude-missing-transcript-fork",
        method::SESSION_FORK,
        serde_json::json!({
            "session_id": rig.session.id,
            "cwd_mode": "same",
            "cols": 80,
            "rows": 24
        }),
    );
    let response = exchange(&mut rig.control, &request).await;
    let sessions: Vec<SessionInfo> = serde_json::from_value(ok_payload(
        exchange(
            &mut rig.control,
            &Request::make("list-after-refused-fork", method::SESSION_LIST, Value::Null),
        )
        .await,
    ))
    .expect("session list after refused fork");
    let argv_after = std::fs::read(&rig.argv_log).expect("argv after fork attempt");
    rig.finish(None).await;

    let Err(error) = response.into_result() else {
        panic!("missing transcript must refuse fork");
    };
    assert_eq!(error.code, "agent_native_reference_missing");
    assert_eq!(sessions.len(), 1, "refused fork cannot register a child");
    assert_eq!(argv_after, argv_before, "refused fork cannot launch");
}

#[tokio::test]
async fn claude_symlinked_transcript_refuses_resume_before_launch() {
    let mut rig = ClaudeRig::new("claude-symlinked-transcript").await;
    let project = rig.config_home.join("projects/fixture");
    std::fs::create_dir_all(&project).expect("create fixture transcript directory");
    let outside = rig.config_home.join("other-conversation.jsonl");
    std::fs::write(&outside, "{}\n").expect("write decoy transcript");
    std::os::unix::fs::symlink(&outside, project.join(format!("{LAUNCH}.jsonl")))
        .expect("link decoy transcript");
    rig.report("startup", LAUNCH).await;
    let unverified = inspect_session(&mut rig.control, &rig.session.id).await;
    assert_eq!(unverified.native_last_activity_at, None);
    let id = rig.session.id.clone();
    let stopped = rig.stop(&id).await;
    let argv_before = std::fs::read(&rig.argv_log).expect("initial launch argv");
    let response = rig.resume().await;
    let after = inspect_session(&mut rig.control, &id).await;
    let argv_after = std::fs::read(&rig.argv_log).expect("argv after recovery attempt");
    rig.finish(None).await;

    let Err(error) = response.into_result() else {
        panic!("symlinked transcript must refuse recovery");
    };
    assert_eq!(error.code, "agent_native_reference_missing");
    assert_eq!(after.runtime, stopped.runtime);
    assert_eq!(argv_after, argv_before, "symlink cannot launch recovery");
}

#[tokio::test]
async fn claude_fork_child_does_not_inherit_the_source_recovery_target() {
    let mut rig = ClaudeRig::new("claude-fork-target").await;
    rig.transcript(LAUNCH);
    rig.transcript(CLEAR);
    rig.report("startup", LAUNCH).await;
    rig.report("clear", CLEAR).await;
    let request = Request::make(
        "claude-recovery-fork",
        method::SESSION_FORK,
        serde_json::json!({
            "session_id": rig.session.id,
            "cwd_mode": "same",
            "cols": 80,
            "rows": 24
        }),
    );
    let forked: protocol::SessionForkResult =
        serde_json::from_value(ok_payload(exchange(&mut rig.control, &request).await))
            .expect("fork Claude conversation");
    let argv = read_file_until(&rig.argv_log, b"--fork-session").await;
    let child_id = forked.session.id.clone();
    rig.finish(Some(child_id)).await;

    assert!(
        String::from_utf8_lossy(&argv).contains(&format!("--resume {CLEAR} --fork-session")),
        "fork must read the source's current verified conversation"
    );
    assert_eq!(
        forked.session.native_session_id, None,
        "a fork cannot resume its source conversation as its own"
    );
}

impl ClaudeRig {
    /// Rewrites the completed generation journal's native-reference claim into
    /// a newer conversation switch of the session's launch process that the
    /// durable record has not imported.
    ///
    /// A healthy worker journals only launch-verified claims, and a live
    /// daemon imports verified ones, so a newer switch claim that persists in
    /// the journal while the record keeps the older target is not producible
    /// through the hook path alone: recovery must refuse the state the issue
    /// observed across daemon lateness and lost resumes.
    ///
    /// The claim keeps the provider, reference kind and process identity of
    /// the journaled startup report, so it reads as journal-shaped evidence;
    /// only its sequence is moved past every accepted report and its
    /// conversation is switched.
    ///
    /// hermetic-allowed: #421 journal evidence is private to the session's
    /// real worker state root inside this fixture.
    fn rejournal_newer_switch(&mut self, reference: &str) {
        let journal = self.journal_path();
        let raw = std::fs::read(&journal).expect("read the terminal worker journal");
        let mut value: serde_json::Value =
            serde_json::from_slice(&raw).expect("the journal the worker wrote is valid JSON");
        let claim = value
            .get_mut("native_reference_claim")
            .and_then(serde_json::Value::as_object_mut)
            .expect("the verified startup report is journaled as a native reference");
        assert!(
            claim.get("sequence").is_some_and(|sequence| {
                sequence
                    .as_u64()
                    .is_some_and(|sequence| sequence < u64::MAX)
            }),
            "the startup report places the switch after the accepted ordering: {claim:?}"
        );
        claim.insert("sequence".into(), serde_json::json!(u64::MAX));
        claim.insert("native_reference".into(), serde_json::json!(reference));
        std::fs::write(
            &journal,
            serde_json::to_vec(&value).expect("journal serializes"),
        )
        .expect("write the journal with the newer switch claim");
        // Keep the journal file owner-private as the trusted reader requires.
        std::fs::set_permissions(
            &journal,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
        )
        .expect("restore journal permissions");
    }
}

#[tokio::test]
async fn claude_newer_unverified_switch_claim_must_not_resume_the_older_verified_target() {
    let mut rig = ClaudeRig::new("claude-unverified-switch").await;
    rig.transcript(LAUNCH);
    rig.report("startup", LAUNCH).await;
    let id = rig.session.id.clone();
    let stopped = rig.stop(&id).await;
    let argv_before = std::fs::read(&rig.argv_log).expect("initial launch argv");
    rig.rejournal_newer_switch(SELECTED);
    let response = rig.resume().await;
    let after = inspect_session(&mut rig.control, &id).await;
    let argv_after = std::fs::read(&rig.argv_log).expect("argv after recovery attempt");
    rig.finish(None).await;

    let Err(error) = response.into_result() else {
        panic!("a newer unverified switch claim must refuse recovery");
    };
    assert_eq!(error.code, "native_identity_uncertain");
    assert_eq!(
        after
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.worker_instance_id.as_deref()),
        stopped
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.worker_instance_id.as_deref()),
        "the refusal must not create a new worker generation"
    );
    assert_eq!(
        argv_after, argv_before,
        "recovery must not relaunch into the older verified target or into the switch"
    );
}

#[tokio::test]
async fn claude_newer_unverified_switch_claim_refuses_fork_before_creating_a_child() {
    let mut rig = ClaudeRig::new("claude-unverified-fork-switch").await;
    rig.transcript(LAUNCH);
    rig.report("startup", LAUNCH).await;
    let argv_before = std::fs::read(&rig.argv_log).expect("initial launch argv");
    rig.rejournal_newer_switch(SELECTED);
    let request = Request::make(
        "claude-unverified-switch-fork",
        method::SESSION_FORK,
        serde_json::json!({
            "session_id": rig.session.id,
            "cwd_mode": "same",
            "cols": 80,
            "rows": 24
        }),
    );
    let response = exchange(&mut rig.control, &request).await;
    let sessions: Vec<SessionInfo> = serde_json::from_value(ok_payload(
        exchange(
            &mut rig.control,
            &Request::make("list-after-refused-fork", method::SESSION_LIST, Value::Null),
        )
        .await,
    ))
    .expect("session list after refused fork");
    let argv_after = std::fs::read(&rig.argv_log).expect("argv after fork attempt");
    rig.finish(None).await;

    let Err(error) = response.into_result() else {
        panic!("a newer unverified switch claim must refuse fork");
    };
    assert_eq!(error.code, "native_identity_uncertain");
    assert_eq!(sessions.len(), 1, "refused fork cannot register a child");
    assert_eq!(argv_after, argv_before, "refused fork cannot launch");
}

async fn assert_unavailable_journal_refuses_recovery(
    tag: &str,
    rewrite: impl FnOnce(&[u8]) -> Vec<u8>,
) {
    let mut rig = ClaudeRig::new(tag).await;
    rig.transcript(LAUNCH);
    rig.report("startup", LAUNCH).await;
    let id = rig.session.id.clone();
    let stopped = rig.stop(&id).await;
    let journal = rig.journal_path();
    let original = std::fs::read(&journal).expect("read exact worker generation journal");
    std::fs::write(&journal, rewrite(&original)).expect("rewrite worker journal evidence");
    let argv_before = std::fs::read(&rig.argv_log).expect("initial launch argv");

    let resume = rig.resume().await;
    let fork = exchange(
        &mut rig.control,
        &Request::make(
            "fork-with-unavailable-journal",
            method::SESSION_FORK,
            serde_json::json!({
                "session_id": id,
                "cwd_mode": "same",
                "cols": 80,
                "rows": 24
            }),
        ),
    )
    .await;
    let after = inspect_session(&mut rig.control, &id).await;
    let argv_after = std::fs::read(&rig.argv_log).expect("argv after refused recovery");
    std::fs::write(&journal, original).expect("restore worker journal for cleanup");
    let sessions: Vec<SessionInfo> = serde_json::from_value(ok_payload(
        exchange(
            &mut rig.control,
            &Request::make(
                "list-after-unavailable-journal",
                method::SESSION_LIST,
                Value::Null,
            ),
        )
        .await,
    ))
    .expect("session list after refused fork");
    rig.finish(None).await;

    for response in [resume, fork] {
        let Err(error) = response.into_result() else {
            panic!("unavailable generation evidence must refuse resume and fork");
        };
        assert_eq!(error.code, "native_identity_evidence_unavailable");
        assert!(error.recover.is_some(), "recovery must offer a retry hint");
    }
    assert_eq!(sessions.len(), 1, "refused fork cannot register a child");
    assert_eq!(
        after.runtime, stopped.runtime,
        "resume cannot mint a generation"
    );
    assert_eq!(
        argv_after, argv_before,
        "neither request can launch an agent"
    );
}

#[tokio::test]
async fn claude_corrupt_exact_generation_journal_refuses_resume_and_fork() {
    assert_unavailable_journal_refuses_recovery("claude-corrupt-recovery-journal", |_original| {
        b"{ invalid journal".to_vec()
    })
    .await;
}

#[tokio::test]
async fn claude_mismatched_generation_instance_refuses_resume_and_fork() {
    assert_unavailable_journal_refuses_recovery("claude-mismatched-recovery-journal", |original| {
        let mut journal: serde_json::Value =
            serde_json::from_slice(original).expect("real worker journal decodes");
        journal["runtime_id"] = serde_json::json!("different-worker-instance");
        serde_json::to_vec(&journal).expect("mismatched journal serializes")
    })
    .await;
}

#[tokio::test]
async fn claude_mismatched_durable_worker_refuses_resume_and_fork() {
    let mut rig = ClaudeRig::new("claude-mismatched-durable-worker").await;
    rig.transcript(LAUNCH);
    rig.report("startup", LAUNCH).await;
    let id = rig.session.id.clone();
    let stopped = rig.stop(&id).await;
    let original_worker = stopped
        .runtime
        .as_ref()
        .and_then(|runtime| runtime.worker_id.as_deref())
        .expect("stopped runtime names its worker")
        .to_owned();
    rig.rewrite_durable_worker_id(&original_worker, "different-worker");
    let argv_before = std::fs::read(&rig.argv_log).expect("initial launch argv");

    let resume = rig.resume().await;
    let fork = exchange(
        &mut rig.control,
        &Request::make(
            "fork-with-mismatched-durable-worker",
            method::SESSION_FORK,
            serde_json::json!({
                "session_id": id,
                "cwd_mode": "same",
                "cols": 80,
                "rows": 24
            }),
        ),
    )
    .await;
    let after = inspect_session(&mut rig.control, &id).await;
    let argv_after = std::fs::read(&rig.argv_log).expect("argv after refused recovery");
    if !resume.is_ok() {
        rig.rewrite_durable_worker_id("different-worker", &original_worker);
    }
    let sessions = listed_sessions(&mut rig.control).await;
    let child = sessions
        .iter()
        .find(|session| session.id != id)
        .map(|session| session.id.clone());
    rig.finish(child).await;

    for response in [resume, fork] {
        let Err(error) = response.into_result() else {
            panic!("mismatched durable worker must refuse resume and fork");
        };
        assert_eq!(error.code, "native_identity_evidence_unavailable");
    }
    assert_eq!(sessions.len(), 1, "refused fork cannot register a child");
    assert_eq!(
        after.runtime, stopped.runtime,
        "resume cannot mint a generation"
    );
    assert_eq!(
        argv_after, argv_before,
        "neither request can launch an agent"
    );
}
