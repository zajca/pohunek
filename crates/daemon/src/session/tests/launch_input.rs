//! Per-launch input of a runtime's contributor across create, resume and
//! removal, driven through a fixture agent that logs its argv and environment.

// Rust guideline compliant 2026-10-27

use std::os::unix::fs::symlink;

use protocol::ProtocolError;

use super::native_recovery::Rig;
use super::*;
use crate::integration::launch_input::{
    FileKind, LaunchContributor, LaunchInput, LaunchInputContext, LaunchKind, PrivateFile,
};

/// Synthetic conversation reference that cannot name an operator conversation.
const NATIVE_ID: &str = "fixture-launch-input-conversation";

/// Overlay file name the fixture contributor asks for.
const OVERLAY: &str = "overlay.json";

/// Executable file the fixture contributor also asks for; its bytes name the
/// launch that wrote it.
const TOOL: &str = "tool.sh";

/// Bytes of the overlay file.
const OVERLAY_BYTES: &[u8] = b"{\"fixture\":true}\n";

/// What the fixture contributor saw in one launch.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    kind: LaunchKind,
    args: Vec<String>,
    overlay: PathBuf,
    sees_base_env: bool,
}

/// A contributor that adds an overlay file, an environment pair naming it and
/// two root arguments, and records the launches it saw.
#[derive(Debug, Default)]
struct Fixture {
    seen: Mutex<Vec<Seen>>,
    /// Contribution returned verbatim for a resume, instead of the usual one.
    resume_input: Mutex<Option<LaunchInput>>,
    /// Refuse a resume with this code.
    refuse_resume: Mutex<Option<String>>,
}

impl LaunchContributor for Fixture {
    fn contribute(&self, ctx: &LaunchInputContext<'_>) -> Result<LaunchInput, ProtocolError> {
        self.seen.lock().expect("seen").push(Seen {
            kind: ctx.kind,
            args: ctx.args.to_vec(),
            overlay: ctx.file_path(OVERLAY),
            sees_base_env: ctx.env.iter().any(|(name, _)| name == "HOME")
                && ctx.env.iter().any(|(name, _)| name == "TMPDIR"),
        });
        if ctx.kind == LaunchKind::Resume {
            if let Some(code) = self.refuse_resume.lock().expect("refusal").clone() {
                return Err(ProtocolError::new(
                    protocol::ErrorClass::Runtime,
                    code,
                    "the fixture contributor refuses this launch",
                    None,
                ));
            }
        }
        if ctx.kind == LaunchKind::Resume {
            if let Some(input) = self.resume_input.lock().expect("input").clone() {
                return Ok(input);
            }
        }
        let launches = self.seen.lock().expect("seen").len();
        let placeholder = format!("{{file:{OVERLAY}}}");
        Ok(LaunchInput {
            env: vec![("TEST_LAUNCH_INPUT".to_owned(), placeholder.clone())],
            root_args: vec!["--overlay".to_owned(), placeholder],
            files: vec![
                PrivateFile {
                    name: OVERLAY.to_owned(),
                    bytes: OVERLAY_BYTES.to_vec(),
                    kind: FileKind::Data,
                },
                PrivateFile {
                    name: TOOL.to_owned(),
                    bytes: format!("#!/bin/sh\n# launch {launches}\n").into_bytes(),
                    kind: FileKind::Executable,
                },
            ],
        })
    }
}

/// Installs a fresh [`Fixture`] as this thread's contributor.
fn install_fixture() -> &'static Fixture {
    let fixture: &'static Fixture = Box::leak(Box::default());
    crate::test_support::set_thread_launch_contributor("codex-hook-v1", Some(fixture));
    fixture
}

/// The overlay path the daemon gives the session `id`.
fn overlay_path(rig: &Rig) -> PathBuf {
    rig.config
        .worker_state_root
        .clone()
        .expect("worker state root")
        .join(&rig.session.id.0)
        .join(pohunek_paths::launch_input_file_name(OVERLAY))
}

/// The lines of the launch log from the first one on, unchanged.
fn launch_log(rig: &Rig) -> String {
    fs::read_to_string(&rig.launches).unwrap_or_default()
}

/// Reports the conversation, settles it and stops the session, so it can resume.
async fn stop_with_reference(rig: &Rig) {
    let snapshot = rig.report(NATIVE_ID).await;
    rig.settle(&snapshot).await;
    rig.finish().await;
}

#[tokio::test]
async fn a_create_launch_gets_the_contributed_env_args_and_private_file() {
    let fixture = install_fixture();
    let rig = Rig::new("launch-input-create", true).await;
    let path = overlay_path(&rig);

    let log = wait_for_file_contains(&rig.launches, "env=").await;
    assert!(
        log.contains(&format!("--overlay {}", path.display())),
        "root args reach the agent: {log}"
    );
    assert!(
        log.contains(&format!("env={}", path.display())),
        "the env pair names the private file: {log}"
    );
    assert_eq!(fs::read(&path).expect("overlay"), OVERLAY_BYTES);
    let mode = fs::metadata(&path)
        .expect("overlay metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "a data file is owner read/write");
    let tool = path.with_file_name(pohunek_paths::launch_input_file_name(TOOL));
    let tool_mode = fs::metadata(&tool)
        .expect("tool metadata")
        .permissions()
        .mode();
    assert_eq!(
        tool_mode & 0o777,
        0o500,
        "an executable is owner read/execute"
    );
    let seen = fixture.seen.lock().expect("seen").clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].kind, LaunchKind::Create);
    assert!(
        seen[0].sees_base_env,
        "the context env holds the inherited base and the launch extras"
    );
    assert_eq!(
        seen[0].overlay, path,
        "the context names the path core writes the file at"
    );
    rig.finish().await;
    rig.remove().await;
}

#[tokio::test]
async fn a_resume_applies_the_contribution_once_and_never_persists_it() {
    let fixture = install_fixture();
    let rig = Rig::new("launch-input-resume", true).await;
    let path = overlay_path(&rig);
    stop_with_reference(&rig).await;

    for round in 1..=2 {
        rig.registry
            .resume(&rig.session.id)
            .await
            .expect("resume the conversation");
        let expected = format!("--overlay {} resume {NATIVE_ID}", path.display());
        wait_until("the relaunch to log its argv", || async {
            (launch_log(&rig).matches(&expected).count() == round).then_some(())
        })
        .await;
        rig.finish().await;
    }

    for line in launch_log(&rig)
        .lines()
        .filter(|line| line.starts_with("--overlay"))
    {
        assert_eq!(
            line.matches("--overlay").count(),
            1,
            "once per relaunch: {line}"
        );
    }
    let tool = overlay_path(&rig).with_file_name(pohunek_paths::launch_input_file_name(TOOL));
    assert_eq!(
        fs::read_to_string(&tool).expect("tool"),
        "#!/bin/sh\n# launch 3\n",
        "every launch rewrites the files"
    );
    let seen = fixture.seen.lock().expect("seen").clone();
    let kinds: Vec<LaunchKind> = seen.iter().map(|seen| seen.kind).collect();
    assert_eq!(
        kinds,
        [LaunchKind::Create, LaunchKind::Resume, LaunchKind::Resume]
    );
    assert!(
        seen.iter()
            .all(|seen| seen.args.iter().all(|arg| arg != "--overlay")),
        "the frozen args handed to the contributor never hold a contribution"
    );
    let record = format!("{:?}", rig.durable_record());
    assert!(
        !record.contains("--overlay"),
        "the stored launch snapshot holds no contribution: {record}"
    );
    rig.remove().await;
}

#[tokio::test]
async fn a_refused_resume_launches_nothing_and_keeps_the_session_resumable() {
    let fixture = install_fixture();
    let rig = Rig::new("launch-input-refused", true).await;
    stop_with_reference(&rig).await;
    let before = launch_log(&rig);

    *fixture.refuse_resume.lock().expect("refusal") = Some("fixture_refusal".to_owned());
    let error = rig
        .registry
        .resume(&rig.session.id)
        .await
        .expect_err("the contributor refuses");
    assert_eq!(error.code, "fixture_refusal");
    assert_eq!(launch_log(&rig), before, "no agent was launched");

    *fixture.refuse_resume.lock().expect("refusal") = None;
    rig.registry
        .resume(&rig.session.id)
        .await
        .expect("a later resume succeeds");
    wait_for_file_contains(&rig.launches, &format!("resume {NATIVE_ID}")).await;
    rig.finish().await;
    rig.remove().await;
}

/// A contribution with one file `overlay.json` and the given parts.
fn input(env: Vec<(&str, String)>, root_args: Vec<String>, files: Vec<PrivateFile>) -> LaunchInput {
    LaunchInput {
        env: env.into_iter().map(|(n, v)| (n.to_owned(), v)).collect(),
        root_args,
        files,
    }
}

fn file(name: &str, bytes: Vec<u8>) -> PrivateFile {
    PrivateFile {
        name: name.to_owned(),
        bytes,
        kind: FileKind::Data,
    }
}

#[tokio::test]
async fn every_bound_of_a_contribution_refuses_the_launch() {
    let fixture = install_fixture();
    let rig = Rig::new("launch-input-hostile", true).await;
    stop_with_reference(&rig).await;
    let before = launch_log(&rig);
    let ok = || file(OVERLAY, b"{}".to_vec());
    let mut hostile: Vec<(&str, LaunchInput)> = Vec::new();
    for name in ["../escape", "UPPER", "a/b", ".hidden", "", "a..b", "trail."] {
        hostile.push((name, input(vec![], vec![], vec![file(name, vec![])])));
    }
    hostile.extend([
        (
            "unknown placeholder",
            input(vec![], vec!["{file:nope}".into()], vec![ok()]),
        ),
        (
            "unterminated placeholder",
            input(vec![("A", "{file:overlay.json".into())], vec![], vec![ok()]),
        ),
        (
            "oversized file",
            input(vec![], vec![], vec![file(OVERLAY, vec![0; 128 * 1024 + 1])]),
        ),
        (
            "too many files",
            input(
                vec![],
                vec![],
                (0..5).map(|n| file(&format!("f{n}"), vec![])).collect(),
            ),
        ),
        ("duplicate file", input(vec![], vec![], vec![ok(), ok()])),
        (
            "too many env pairs",
            input(
                ["A0", "A1", "A2", "A3", "A4", "A5", "A6", "A7", "A8"]
                    .into_iter()
                    .map(|name| (name, String::new()))
                    .collect(),
                vec![],
                vec![],
            ),
        ),
        (
            "too many args",
            input(vec![], (0..9).map(|n| n.to_string()).collect(), vec![]),
        ),
        (
            "reserved env name",
            input(vec![("POHUNEK_X", "1".into())], vec![], vec![]),
        ),
        (
            "lower-case env name",
            input(vec![("path", "1".into())], vec![], vec![]),
        ),
        (
            "env name with a digit first",
            input(vec![("1A", "1".into())], vec![], vec![]),
        ),
        (
            "NUL in a value",
            input(vec![("A", "a\0b".into())], vec![], vec![]),
        ),
        (
            "overlong value",
            input(vec![("A", "x".repeat(4097))], vec![], vec![]),
        ),
    ]);

    for (label, contribution) in hostile {
        *fixture.resume_input.lock().expect("input") = Some(contribution);
        let error = rig
            .registry
            .resume(&rig.session.id)
            .await
            .expect_err("a contribution outside the bounds is refused");
        assert_eq!(error.code, "launch_input_invalid", "{label}");
    }
    assert_eq!(launch_log(&rig), before, "no agent was launched");
    rig.remove().await;
}

#[tokio::test]
async fn a_file_no_longer_contributed_is_removed_on_the_next_launch() {
    let fixture = install_fixture();
    let rig = Rig::new("launch-input-stale", true).await;
    let overlay = overlay_path(&rig);
    let tool = overlay.with_file_name(pohunek_paths::launch_input_file_name(TOOL));
    stop_with_reference(&rig).await;
    assert!(overlay.exists() && tool.exists());
    *fixture.resume_input.lock().expect("input") = Some(input(
        vec![],
        vec![],
        vec![file("other.json", b"{}".to_vec())],
    ));

    rig.registry.resume(&rig.session.id).await.expect("resume");
    wait_for_file_contains(&rig.launches, &format!("resume {NATIVE_ID}")).await;

    assert!(
        !overlay.exists() && !tool.exists(),
        "earlier files are unlinked"
    );
    assert!(overlay
        .with_file_name(pohunek_paths::launch_input_file_name("other.json"))
        .exists());
    rig.finish().await;
    rig.remove().await;
}

#[tokio::test]
async fn a_relaunch_served_by_another_definition_does_not_ask_the_original_contributor() {
    let fixture = install_fixture();
    let mut rig = Rig::new("launch-input-unserved", true).await;
    stop_with_reference(&rig).await;
    rig.serve_with_handler("claude-hook-v1").await;

    rig.registry.resume(&rig.session.id).await.expect("resume");
    let log = wait_for_file_contains(&rig.launches, &format!("resume {NATIVE_ID}")).await;

    let line = log.lines().rev().nth(1).expect("relaunch argv line");
    assert_eq!(line, format!("resume {NATIVE_ID}"), "no root args: {log}");
    assert_eq!(log.lines().next_back(), Some("env="));
    assert_eq!(
        fixture.seen.lock().expect("seen").len(),
        1,
        "only the create asked"
    );
    rig.finish().await;
    rig.remove().await;
}

#[tokio::test]
async fn a_planted_symlink_is_refused_and_never_followed() {
    install_fixture();
    let rig = Rig::new("launch-input-symlink", true).await;
    let path = overlay_path(&rig);
    stop_with_reference(&rig).await;
    let victim = path.with_file_name("victim.txt");
    fs::write(&victim, b"untouched").expect("victim");
    fs::remove_file(&path).expect("remove the earlier overlay");
    symlink(&victim, &path).expect("plant the symlink");
    let before = launch_log(&rig);

    let error = rig
        .registry
        .resume(&rig.session.id)
        .await
        .expect_err("a symlink at the file name is refused");

    assert_eq!(error.code, "launch_input_unavailable");
    assert_eq!(fs::read(&victim).expect("victim"), b"untouched");
    assert_eq!(launch_log(&rig), before, "no agent was launched");
    rig.remove().await;
}

#[tokio::test]
async fn removing_the_session_deletes_its_launch_input_without_a_tombstone() {
    install_fixture();
    let rig = Rig::new("launch-input-removal", true).await;
    let path = overlay_path(&rig);
    wait_for_file_contains(&rig.launches, "env=").await;
    assert!(path.exists());
    rig.finish().await;
    rig.remove().await;

    let state_root = rig.config.worker_state_root.clone().expect("state root");
    wait_until("the session directory to disappear", || async {
        let tombstone = fs::read_dir(&state_root).ok()?.flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(pohunek_paths::WORKER_TOMBSTONE_PREFIX)
        });
        (!path.exists() && !tombstone).then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_fork_gets_its_own_contribution_and_the_source_snapshot_stays_clean() {
    let fixture = install_fixture();
    let rig = Rig::new("launch-input-fork", true).await;
    let source_path = overlay_path(&rig);
    let snapshot = rig.report(NATIVE_ID).await;
    rig.settle(&snapshot).await;

    let forked = rig
        .registry
        .fork(protocol::SessionForkParams {
            session_id: rig.session.id.clone(),
            name: None,
            cwd_mode: protocol::ForkCwdMode::default(),
            cols: 80,
            rows: 24,
            accept_profile_change: false,
        })
        .await
        .expect("fork the conversation");

    let fork_path = source_path.with_file_name(pohunek_paths::launch_input_file_name(OVERLAY));
    let fork_path = fork_path
        .parent()
        .and_then(Path::parent)
        .expect("state root")
        .join(&forked.id.0)
        .join(pohunek_paths::launch_input_file_name(OVERLAY));
    let expected = format!("--overlay {} fork {NATIVE_ID}", fork_path.display());
    wait_for_file_contains(&rig.launches, &expected).await;
    assert_ne!(
        fork_path, source_path,
        "the fork has a directory of its own"
    );
    assert_eq!(fs::read(&fork_path).expect("fork overlay"), OVERLAY_BYTES);
    let kinds: Vec<LaunchKind> = fixture
        .seen
        .lock()
        .expect("seen")
        .iter()
        .map(|seen| seen.kind)
        .collect();
    assert_eq!(kinds, [LaunchKind::Create, LaunchKind::Fork]);
    let record = format!("{:?}", rig.durable_record());
    assert!(
        !record.contains("--overlay"),
        "the source snapshot is clean: {record}"
    );
    rig.registry.stop(&forked.id).await.expect("stop the fork");
    rig.registry.settle_transactions().await;
    rig.registry
        .remove(&forked.id)
        .await
        .expect("remove the fork");
    rig.finish().await;
    rig.remove().await;
}
