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

/// Bytes of the overlay file.
const OVERLAY_BYTES: &[u8] = b"{\"fixture\":true}\n";

/// What the fixture contributor saw in one launch.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    kind: LaunchKind,
    args: Vec<String>,
}

/// A contributor that adds an overlay file, an environment pair naming it and
/// two root arguments, and records the launches it saw.
#[derive(Debug, Default)]
struct Fixture {
    seen: Mutex<Vec<Seen>>,
    /// Name of the file to ask for on a resume, instead of [`OVERLAY`].
    resume_file_name: Mutex<Option<String>>,
    /// Refuse a resume with this code.
    refuse_resume: Mutex<Option<String>>,
}

impl LaunchContributor for Fixture {
    fn contribute(&self, ctx: &LaunchInputContext<'_>) -> Result<LaunchInput, ProtocolError> {
        self.seen.lock().expect("seen").push(Seen {
            kind: ctx.kind,
            args: ctx.args.to_vec(),
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
        let name = if ctx.kind == LaunchKind::Resume {
            self.resume_file_name.lock().expect("name").clone()
        } else {
            None
        }
        .unwrap_or_else(|| OVERLAY.to_owned());
        let placeholder = format!("{{file:{name}}}");
        Ok(LaunchInput {
            env: vec![("TEST_LAUNCH_INPUT".to_owned(), placeholder.clone())],
            root_args: vec!["--overlay".to_owned(), placeholder],
            files: vec![PrivateFile {
                name,
                bytes: OVERLAY_BYTES.to_vec(),
                kind: FileKind::Data,
            }],
        })
    }
}

/// Installs a fresh [`Fixture`] as this thread's contributor.
fn install_fixture() -> &'static Fixture {
    let fixture: &'static Fixture = Box::leak(Box::default());
    crate::test_support::set_thread_launch_contributor(fixture);
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
    let seen = fixture.seen.lock().expect("seen").clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].kind, LaunchKind::Create);
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

#[tokio::test]
async fn a_hostile_contribution_is_refused_before_any_launch() {
    let fixture = install_fixture();
    let rig = Rig::new("launch-input-hostile", true).await;
    stop_with_reference(&rig).await;
    let before = launch_log(&rig);

    for name in ["../escape", "UPPER", "a/b", ".hidden", ""] {
        *fixture.resume_file_name.lock().expect("name") = Some(name.to_owned());
        let error = rig
            .registry
            .resume(&rig.session.id)
            .await
            .expect_err("a hostile file name is refused");
        assert_eq!(error.code, "launch_input_invalid", "{name:?}");
    }
    assert_eq!(launch_log(&rig), before, "no agent was launched");
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
