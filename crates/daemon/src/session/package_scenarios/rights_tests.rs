//! Where the rights classifier places a caller across a session's launch and
//! a daemon restart, and which rights a restored session holds.
//!
//! A session's worker is the ancestor of every process of the session, so the
//! daemon records it before the worker forks the PTY child. A restart adopts
//! the live worker and takes its identity from it; a live session whose root
//! could not be verified leaves callers outside every tree unattributed.

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

/// The identity of the running test process, standing for a caller outside
/// every session tree.
fn outside_caller() -> ControlPeer {
    let pid = std::process::id();
    let identity = HostInspector::new()
        .identity(pid)
        .expect("the host reads the test process")
        .expect("the test process runs");
    ControlPeer::for_test(identity)
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
    let peer = ControlPeer::for_test(worker);
    let (class, keep) = fixture.registry.classify_caller(&peer).await;
    assert_eq!(class, CallerClass::Session(launching.clone()));
    assert!(keep);

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
async fn a_live_session_without_a_verified_root_leaves_outside_callers_unattributed() {
    let fixture = Fixture::new("rights-unverified");
    fixture.write_profile(PROFILE, &profile_text("[]"));
    let id = create(&fixture).await;
    let outside = outside_caller();
    assert!(
        fixture.registry.unverified_root_sessions().await.is_empty(),
        "a launched session has verified roots"
    );

    let mut sessions = fixture.registry.inner.sessions.lock().await;
    let entry = sessions.get_mut(&id).expect("the session is held");
    entry.root_process = None;
    entry.worker_root = None;
    drop(sessions);
    let (class, keep) = fixture.registry.classify_caller(&outside).await;
    assert_eq!(class, CallerClass::Unattributed);
    assert!(!keep, "the placement is made again on the next request");
    assert_eq!(
        fixture.registry.unverified_root_sessions().await,
        vec![id.clone()]
    );
    let check = crate::doctor::session_rights_check(false, std::slice::from_ref(&id));
    assert_eq!(check.status, protocol::DoctorStatus::Fail);
    assert!(check.detail.contains(&id.0), "{}", check.detail);

    fixture.registry.stop(&id).await.expect("stop the session");
}
