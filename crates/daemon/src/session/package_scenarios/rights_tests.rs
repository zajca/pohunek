//! Where the rights classifier places a caller across a session's launch and
//! a daemon restart, and which rights a restored session holds.
//!
//! A session's worker is the ancestor of every process of the session, so the
//! daemon records it before the worker forks the PTY child, on a launch and on
//! a resume alike. A restart adopts the live worker and takes its identity from
//! it. The restore paths that leave a session without a worker (a
//! reconnecting, refused or unreachable runtime) are in the reconcile
//! scenarios.

// Rust guideline compliant 2026-11-16

use protocol::SessionId;

use super::test_fixture::Fixture;
use crate::procwatch::{HostInspector, ProcessIdentity, ProcessInspector as _};
use crate::session::rights::CallerClass;
use crate::session::tests::{params, temp_dir, temp_store_path};
use crate::session::ControlPeer;

const PROFILE: &str = "rights-worker";

fn profile_text(spawn_profiles: &str) -> String {
    format!(
        "base = \"shell\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"exec sleep 600\"]\n\n\
         [rights]\nspawn_profiles = {spawn_profiles}\n"
    )
}

/// The processes the worker `pid` forked, read from the kernel; empty where
/// the process table does not list them.
fn children_of(pid: u32) -> Vec<ProcessIdentity> {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return Vec::new();
    };
    let inspector = HostInspector::new();
    tasks
        .flatten()
        .filter_map(|task| std::fs::read_to_string(task.path().join("children")).ok())
        .flat_map(|children| {
            children
                .split_whitespace()
                .filter_map(|child| child.parse::<u32>().ok())
                .collect::<Vec<_>>()
        })
        .filter_map(|child| inspector.identity(child).ok().flatten())
        .collect()
}

/// Asserts the worker and every process it forked are placed in `id`.
async fn assert_worker_tree_is_placed(fixture: &Fixture, id: &SessionId, worker: ProcessIdentity) {
    let (class, keep) = fixture
        .registry
        .classify_caller(&ControlPeer::for_test(worker))
        .await;
    assert_eq!(class, CallerClass::Session(id.clone()));
    assert!(keep);
    if cfg!(target_os = "linux") {
        let children = children_of(worker.pid);
        assert!(
            !children.is_empty(),
            "the worker forked the PTY child before the launch commit"
        );
        for child in children {
            let (class, _keep) = fixture
                .registry
                .classify_caller(&ControlPeer::for_test(child))
                .await;
            assert_eq!(
                class,
                CallerClass::Session(id.clone()),
                "a process below the worker is never the owner"
            );
        }
    }
}

async fn create(fixture: &Fixture) -> SessionId {
    fixture
        .registry
        .create(protocol::SessionNewParams {
            agent: PROFILE.to_owned(),
            cwd: Some(temp_dir("rights-cwd")),
            ..params()
        })
        .await
        .expect("a session of the rights profile")
        .id
}

/// The roots a live entry holds.
async fn roots_of(
    fixture: &Fixture,
    id: &SessionId,
) -> (Option<ProcessIdentity>, Option<ProcessIdentity>) {
    let sessions = fixture.registry.inner.sessions.lock().await;
    let entry = sessions.get(id).expect("the session is held");
    (entry.root_process, entry.worker_root)
}

#[tokio::test]
async fn a_process_below_a_launching_worker_is_placed_in_its_session() {
    let fixture = Fixture::new("rights-launch-window");
    fixture.write_profile(PROFILE, &profile_text("[]"));
    let gate = fixture.registry.hold_next_launch_commit();
    let creating = tokio::spawn({
        let registry = fixture.registry.clone();
        async move {
            registry
                .create(protocol::SessionNewParams {
                    agent: PROFILE.to_owned(),
                    cwd: Some(temp_dir("rights-cwd")),
                    ..params()
                })
                .await
        }
    });
    tokio::time::timeout(
        pohunek_test_support::wait::HANG_GUARD,
        gate.entered.notified(),
    )
    .await
    .expect("the launch parks before its entry is installed");

    let launches = fixture.registry.launching_for_test();
    let [launching] = launches.as_slice() else {
        panic!("one launch is in flight: {launches:?}");
    };
    let worker = fixture
        .registry
        .launch_worker_for_test(launching)
        .expect("the launch recorded its worker before the PTY child ran");
    assert_worker_tree_is_placed(&fixture, launching, worker).await;

    gate.release.notify_one();
    let created = creating
        .await
        .expect("create task joins")
        .expect("the launch completes");
    let (root, worker_root) = roots_of(&fixture, &created.id).await;
    assert!(root.is_some(), "the PTY root is recorded");
    assert_eq!(worker_root, Some(worker));
    fixture
        .registry
        .stop(&created.id)
        .await
        .expect("stop the session");
}

#[tokio::test]
async fn a_restored_session_keeps_its_launch_rights_and_a_verified_root() {
    let store_path = temp_store_path("rights-restore");
    let fixture = Fixture::with_store("rights-restore", store_path);
    fixture.write_profile(PROFILE, &profile_text("[\"narrow\"]"));
    let id = create(&fixture).await;
    let launched = fixture
        .registry
        .session_profile_rights(&id)
        .await
        .expect("the session is held");
    assert_eq!(launched.spawn_profiles, Some(vec!["narrow".to_owned()]));

    // The owner widens the profile while the daemon is down.
    fixture.write_profile(PROFILE, &profile_text("[\"narrow\", \"wide\"]"));
    let restarted = fixture.reopen_with_workers().await;
    restarted
        .registry
        .reconcile_workers()
        .await
        .expect("startup reconciliation");

    let restored = restarted
        .registry
        .session_profile_rights(&id)
        .await
        .expect("the restarted daemon adopts the session");
    assert_eq!(
        restored.spawn_profiles,
        Some(vec!["narrow".to_owned()]),
        "a restart never widens a session's launch rights"
    );
    let (root, worker_root) = roots_of(&restarted, &id).await;
    let root = root.expect("the adopted session has a PTY root");
    assert!(
        worker_root.is_some(),
        "the adopted session has a worker root"
    );
    let (class, _keep) = restarted
        .registry
        .classify_caller(&ControlPeer::for_test(root))
        .await;
    assert!(
        matches!(class, CallerClass::Session(ref held) | CallerClass::OwnerShell(ref held) if *held == id),
        "the verified root places its own tree: {class:?}"
    );

    // Removing the record drops the rights with it.
    restarted
        .registry
        .stop(&id)
        .await
        .expect("stop the adopted session");
    restarted
        .registry
        .remove(&id)
        .await
        .expect("remove the session");
    assert!(restarted
        .registry
        .session_profile_rights(&id)
        .await
        .is_none());
}

#[tokio::test]
async fn a_process_below_a_resumed_worker_is_placed_in_its_session() {
    let (fixture, _package, dir, mut gate, _store) =
        super::tests::session_fixture("rights-resume-window").await;
    let created = fixture
        .registry
        .create(super::tests::new_params(&dir))
        .await
        .expect("create");
    std::io::Write::write_all(&mut gate, b"go\n").expect("release the exit gate");
    fixture
        .registry
        .wait_for_exit(&created.id, pohunek_test_support::wait::HANG_GUARD)
        .await
        .expect("the session exits");

    let hold = fixture.registry.hold_next_launch_commit();
    let resuming = tokio::spawn({
        let registry = fixture.registry.clone();
        let id = created.id.clone();
        async move { registry.resume(&id).await }
    });
    tokio::time::timeout(
        pohunek_test_support::wait::HANG_GUARD,
        hold.entered.notified(),
    )
    .await
    .expect("the resume parks before its entry is installed");

    // The entry of the ended runtime is still registered; the new worker is
    // placed through the launch reservation.
    let worker = fixture
        .registry
        .launch_worker_for_test(&created.id)
        .expect("the relaunch recorded its worker");
    assert_worker_tree_is_placed(&fixture, &created.id, worker).await;

    hold.release.notify_one();
    resuming
        .await
        .expect("resume task joins")
        .expect("the resume completes");
    fixture
        .registry
        .stop(&created.id)
        .await
        .expect("stop the session");
}

/// Rewrites the stored record of `id` as a daemon before launch rights and
/// worker identities were recorded wrote it.
fn strip_recorded_rights(store: &std::path::Path, id: &SessionId) {
    let store = crate::store::Store::new(store.to_path_buf());
    let mut record = store
        .load_sessions()
        .expect("load the sessions")
        .into_iter()
        .find(|record| record.session_id == id.0)
        .expect("the session is stored");
    record.launch_rights = None;
    record.worker_process = None;
    store
        .record_session(&record)
        .expect("rewrite the session record");
}

fn stored_launch_rights(
    store: &std::path::Path,
    id: &SessionId,
) -> Option<crate::store::LaunchRights> {
    crate::store::Store::new(store.to_path_buf())
        .load_sessions()
        .expect("load the sessions")
        .into_iter()
        .find(|record| record.session_id == id.0)
        .expect("the session is stored")
        .launch_rights
}

#[tokio::test]
async fn a_session_recorded_before_launch_rights_takes_the_file_once_and_never_widens() {
    let store_path = temp_store_path("rights-pre-record");
    let fixture = Fixture::with_store("rights-pre-record", store_path.clone());
    fixture.write_profile(PROFILE, &profile_text("[\"narrow\"]"));
    let id = create(&fixture).await;
    strip_recorded_rights(&store_path, &id);
    assert!(stored_launch_rights(&store_path, &id).is_none());

    let restarted = fixture.reopen_with_workers().await;
    restarted
        .registry
        .reconcile_workers()
        .await
        .expect("startup reconciliation");
    let first = restarted
        .registry
        .session_profile_rights(&id)
        .await
        .expect("the restarted daemon adopts the session");
    assert_eq!(
        first.spawn_profiles,
        Some(vec!["narrow".to_owned()]),
        "the first use takes the profile file as it reads"
    );
    assert_eq!(
        stored_launch_rights(&store_path, &id).map(|held| held.spawn_profiles),
        Some(Some(vec!["narrow".to_owned()])),
        "the first-use snapshot is stored at once"
    );

    // A profile widened afterwards never widens the session, not even across
    // another restart.
    restarted.write_profile(PROFILE, &profile_text("[\"narrow\", \"wide\"]"));
    let again = restarted.reopen_with_workers().await;
    again
        .registry
        .reconcile_workers()
        .await
        .expect("startup reconciliation");
    let held = again
        .registry
        .session_profile_rights(&id)
        .await
        .expect("the session is adopted again");
    assert_eq!(held.spawn_profiles, Some(vec!["narrow".to_owned()]));

    again.registry.stop(&id).await.expect("stop the session");
}
