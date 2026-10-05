//! Tests of the host-profile revision frozen into a session: resume and fork
//! relaunch under the profile only while its revision matches, and an owner
//! decision relaunches under the current profile and freezes its revision.
//!
//! Every test drives real session registries over a real agent script. The
//! script records the profile variable the launched child received, so what a
//! relaunch ran under is read from the child, never inferred.

// Rust guideline compliant 2026-10-05

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pohunek_test_support::wait::HANG_GUARD;
use protocol::{
    method, ForkCwdMode, ProtocolError, Request, SessionForkParams, SessionId, SessionNewParams,
};

use super::tests::{
    durable_recovery, hermetic_shell, hook_gate, params, temp_dir, temp_store_path,
    wait_for_file_contains, write_executable,
};
use super::{ProfileChange, SessionRegistry, SessionRegistryConfig};
use crate::agent::host::fixture::{pi_shaped_host, pi_shaped_profile_pin, PI_SHAPED_NO_CHECK};
use crate::agent::host::ProfileRevision;
use crate::api::{handle_request, ControlTransport, DaemonState, HealthInfo};

/// Profile variable the script reports; the value is the profile's mark.
const MARK_VAR: &str = "PROFILE_MARK";

/// A secret-looking mark that must never reach the store.
const SECRET_MARK: &str = "mark-secret-value-v1";

/// The package fixture, an agents directory holding the profile `work`, and
/// the files the recorded launches land in.
struct Frozen {
    /// Name of the profile file and of the agent the sessions launch as.
    name: &'static str,
    dir: PathBuf,
    agents: PathBuf,
    store_path: PathBuf,
    script: PathBuf,
    marker: PathBuf,
    /// Keeps the first run alive until released; dropping it releases it.
    gate: fs::File,
}

impl Frozen {
    fn new(tag: &str) -> Self {
        Self::named(tag, "work")
    }

    /// A fixture whose profile `name` may shadow a runtime id.
    fn named(tag: &str, name: &'static str) -> Self {
        let dir = temp_dir(&format!("{tag}-run"));
        let agents = temp_dir(&format!("{tag}-agents"));
        let marker = dir.join("launches.txt");
        let script = dir.join("marked-agent");
        let gate_path = dir.join("exit.gate");
        let gate = hook_gate(&gate_path);
        write_executable(
            &script,
            &format!(
                "#!/bin/sh\nprintf 'launch\\n' >> '{marker}'\nprintf 'mark=%s\\n' \"${{{MARK_VAR}-unset}}\" >> '{marker}'\nprintf '%s\\n' \"$@\" >> '{marker}'\ncase \" $* \" in *\" --session-id \"*) read _ < '{gate}'; exit 0 ;; *) sleep 30 ;; esac\n",
                marker = marker.display(),
                gate = gate_path.display(),
            ),
        );
        let frozen = Self {
            name,
            dir,
            agents,
            store_path: temp_store_path(tag),
            script,
            marker,
            gate,
        };
        frozen.write_profile(SECRET_MARK);
        frozen
    }

    /// Writes the profile `work` with `mark` as its environment variable.
    fn write_profile(&self, mark: &str) {
        fs::write(
            self.agents.join(format!("{}.toml", self.name)),
            format!(
                "base = \"pi\"\n{}program = \"{}\"\n[env]\n{MARK_VAR} = \"{mark}\"\n",
                pi_shaped_profile_pin(),
                self.script.display()
            ),
        )
        .expect("write profile");
    }

    fn delete_profile(&self) {
        fs::remove_file(self.agents.join(format!("{}.toml", self.name))).expect("delete profile");
    }

    fn config(&self) -> SessionRegistryConfig {
        SessionRegistryConfig {
            shell_command: hermetic_shell(),
            stop_grace: Duration::from_millis(50),
            store_path: Some(self.store_path.clone()),
            agents_dir: Some(self.agents.clone()),
            ..SessionRegistryConfig::default()
        }
    }

    fn registry_with(&self, config: SessionRegistryConfig) -> SessionRegistry {
        SessionRegistry::new_with_runtimes(config, pi_shaped_host(&self.script, PI_SHAPED_NO_CHECK))
    }

    fn registry(&self) -> SessionRegistry {
        self.registry_with(self.config())
    }

    /// A registry over the same store, as after a daemon restart.
    async fn restarted(&self) -> SessionRegistry {
        let registry = self.registry();
        Box::pin(registry.reconcile_workers())
            .await
            .expect("startup reconciliation");
        registry
    }

    fn new_params(&self) -> SessionNewParams {
        SessionNewParams {
            agent: self.name.to_owned(),
            cwd: Some(self.dir.clone()),
            ..params()
        }
    }

    /// Creates the profile session, lets its first run exit and returns its id.
    async fn created_and_exited(&self, registry: &SessionRegistry) -> SessionId {
        let created = registry
            .create(self.new_params())
            .await
            .expect("create a profile session");
        wait_for_file_contains(&self.marker, "--session-id").await;
        release(&self.gate);
        registry
            .wait_for_exit(&created.id, HANG_GUARD)
            .await
            .expect("session exits");
        created.id
    }

    /// The marks the launches so far received, in launch order.
    fn marks(&self) -> Vec<String> {
        fs::read_to_string(&self.marker)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.strip_prefix("mark=").map(str::to_owned))
            .collect()
    }

    async fn wait_for_launches(&self, count: usize) -> Vec<String> {
        pohunek_test_support::wait::wait_until(
            &format!("{count} launches of the agent"),
            || async { Some(self.marks()).filter(|marks| marks.len() >= count) },
        )
        .await
    }

    fn recovery(&self, id: &SessionId) -> crate::store::ResumeBinding {
        durable_recovery(&self.store_path, id)
    }

    fn frozen_revision(&self, id: &SessionId) -> Option<ProfileRevision> {
        self.recovery(id).profile_revision
    }
}

/// Writes the line a parked first run waits for.
fn release(gate: &fs::File) {
    use std::io::Write as _;
    let mut writer = gate;
    writer.write_all(b"go\n").expect("release the exit gate");
}

fn current_revision(registry: &SessionRegistry) -> ProfileRevision {
    let profiles = &registry.inner.profiles;
    let agent = profiles.resolve_agent("work").expect("profile resolves");
    profiles
        .revision_of(&agent)
        .expect("revision key")
        .expect("a profile has a revision")
}

fn fork_params(id: &SessionId, accept_profile_change: bool) -> SessionForkParams {
    SessionForkParams {
        session_id: id.clone(),
        name: None,
        cwd_mode: ForkCwdMode::Same,
        cols: 80,
        rows: 24,
        accept_profile_change,
    }
}

/// Arms `registry` to park its next resume or fork after the profile resolved.
fn hold_recovery(registry: &SessionRegistry) -> crate::runtime::lifecycle::tests::StartGate {
    let gate = crate::runtime::lifecycle::tests::StartGate {
        session_id: None,
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    };
    *registry
        .inner
        .recovery_hold
        .lock()
        .expect("recovery hold is never poisoned") = Some(gate.clone());
    gate
}

/// The number of agent launches recorded in `marker`.
fn launch_count(marker: &Path) -> usize {
    fs::read_to_string(marker)
        .unwrap_or_default()
        .matches("launch\n")
        .count()
}

#[tokio::test]
async fn a_profile_session_freezes_the_current_revision_at_creation() {
    let frozen = Frozen::new("freeze-create");
    let registry = frozen.registry();

    let id = frozen.created_and_exited(&registry).await;

    assert_eq!(
        frozen.frozen_revision(&id),
        Some(current_revision(&registry)),
        "the durable binding carries the keyed revision of the profile it launched under"
    );
    let raw = fs::read_to_string(&frozen.store_path).expect("read store");
    assert!(
        !raw.contains(SECRET_MARK) && !raw.contains(MARK_VAR),
        "the revision is a MAC: the profile env never reaches the store: {raw}"
    );
}

#[tokio::test]
async fn a_session_without_a_profile_freezes_no_revision_and_resumes_unconditionally() {
    let frozen = Frozen::new("freeze-bare");
    let registry = frozen.registry();
    let created = registry
        .create(SessionNewParams {
            agent: "pi".to_owned(),
            cwd: Some(frozen.dir.clone()),
            ..params()
        })
        .await
        .expect("create a bare runtime session");
    wait_for_file_contains(&frozen.marker, "--session-id").await;
    release(&frozen.gate);
    registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("session exits");

    assert_eq!(frozen.frozen_revision(&created.id), None);

    // A profile that appears under another name changes nothing for it.
    frozen.write_profile("anything");
    registry
        .resume(&created.id)
        .await
        .expect("a session without a profile has nothing to compare");
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn resume_of_an_unchanged_profile_relaunches_under_it_after_a_restart() {
    let frozen = Frozen::new("resume-same");
    let registry = frozen.registry();
    let id = frozen.created_and_exited(&registry).await;
    let revision = frozen.frozen_revision(&id);
    drop(registry);

    let restarted = frozen.restarted().await;
    restarted.resume(&id).await.expect("resume");

    assert_eq!(
        frozen.wait_for_launches(2).await,
        [SECRET_MARK, SECRET_MARK]
    );
    assert_eq!(frozen.frozen_revision(&id), revision, "nothing re-froze");
    let _ = restarted.stop(&id).await;
}

#[tokio::test]
async fn resume_of_an_edited_profile_is_refused_until_the_owner_accepts_it() {
    let frozen = Frozen::new("resume-edit");
    let registry = frozen.registry();
    let id = frozen.created_and_exited(&registry).await;
    let before = frozen.frozen_revision(&id).expect("frozen at creation");
    drop(registry);

    frozen.write_profile("second-account");
    let restarted = frozen.restarted().await;
    let refused = restarted
        .resume(&id)
        .await
        .expect_err("an edited profile is not relaunched silently");

    assert_eq!(refused.code, "agent_profile_changed");
    assert!(
        refused
            .recover
            .as_deref()
            .is_some_and(|hint| hint.contains("--accept-profile-change")),
        "{refused:?}"
    );
    assert!(
        !refused.msg.contains("second-account") && !refused.msg.contains(SECRET_MARK),
        "an error never carries profile values: {refused:?}"
    );
    assert_eq!(
        launch_count(&frozen.marker),
        1,
        "a refusal launches nothing"
    );
    assert_eq!(
        frozen.frozen_revision(&id),
        Some(before.clone()),
        "a refusal leaves the frozen revision alone"
    );

    restarted
        .resume_with(&id, ProfileChange::Accept)
        .await
        .expect("the owner accepts the edited profile");

    assert_eq!(
        frozen.wait_for_launches(2).await,
        [SECRET_MARK, "second-account"],
        "the relaunch runs under the current profile"
    );
    let after = frozen.frozen_revision(&id).expect("re-frozen");
    assert_ne!(after, before);
    assert_eq!(after, current_revision(&restarted));

    // The re-frozen revision is durable: a restart resumes without the override.
    restarted.stop(&id).await.expect("stop");
    drop(restarted);
    let again = frozen.restarted().await;
    again
        .resume(&id)
        .await
        .expect("the accepted profile is the frozen one now");
    assert_eq!(
        frozen.wait_for_launches(3).await,
        [SECRET_MARK, "second-account", "second-account"]
    );
    let _ = again.stop(&id).await;
}

#[tokio::test]
async fn resume_of_a_deleted_profile_is_refused_even_with_the_override() {
    let frozen = Frozen::new("resume-deleted");
    let registry = frozen.registry();
    let id = frozen.created_and_exited(&registry).await;
    drop(registry);

    frozen.delete_profile();
    let restarted = frozen.restarted().await;
    for change in [ProfileChange::Refuse, ProfileChange::Accept] {
        let refused = restarted
            .resume_with(&id, change)
            .await
            .expect_err("a deleted profile cannot be relaunched under");
        assert_eq!(refused.code, "agent_profile_missing", "{change:?}");
    }
    assert_eq!(launch_count(&frozen.marker), 1, "nothing launched");

    // Restoring the same profile text restores the session's revision.
    frozen.write_profile(SECRET_MARK);
    restarted
        .resume(&id)
        .await
        .expect("the restored profile has the frozen revision");
    let _ = restarted.stop(&id).await;
}

#[tokio::test]
async fn a_legacy_binding_without_a_revision_needs_the_override_and_then_freezes() {
    let frozen = Frozen::new("resume-legacy");
    let registry = frozen.registry();
    let id = frozen.created_and_exited(&registry).await;
    drop(registry);

    // The record as an older daemon wrote it: no frozen revision.
    let store = crate::store::Store::new(frozen.store_path.clone());
    let mut record = store
        .load_sessions()
        .expect("load")
        .into_iter()
        .find(|record| record.session_id == id.0)
        .expect("durable record");
    record.recovery.as_mut().expect("recovery").profile_revision = None;
    store.record_session(&record).expect("rewrite the record");
    assert_eq!(frozen.frozen_revision(&id), None);

    let restarted = frozen.restarted().await;
    let refused = restarted
        .resume(&id)
        .await
        .expect_err("a legacy profile session is never relaunched silently");
    assert_eq!(refused.code, "agent_profile_changed");
    assert!(refused.msg.contains("no recorded revision"), "{refused:?}");
    assert_eq!(launch_count(&frozen.marker), 1);

    restarted
        .resume_with(&id, ProfileChange::Accept)
        .await
        .expect("the owner accepts the legacy session");
    assert_eq!(
        frozen.frozen_revision(&id),
        Some(current_revision(&restarted)),
        "the accepted relaunch freezes the current revision"
    );
    let _ = restarted.stop(&id).await;
}

#[tokio::test]
async fn an_unavailable_revision_key_fails_closed_for_profile_sessions_only() {
    let frozen = Frozen::new("key-unavailable");
    let registry = frozen.registry();
    let id = frozen.created_and_exited(&registry).await;
    drop(registry);
    let not_a_directory = frozen.dir.join("not-a-state-dir");
    fs::write(&not_a_directory, "").expect("write a plain file");

    for host_state_dir in [None, Some(not_a_directory)] {
        let config = SessionRegistryConfig {
            host_state_dir: host_state_dir.clone(),
            ..frozen.config()
        };
        let broken = frozen.registry_with(config);
        Box::pin(broken.reconcile_workers())
            .await
            .expect("startup reconciliation");

        for change in [ProfileChange::Refuse, ProfileChange::Accept] {
            let refused = broken
                .resume_with(&id, change)
                .await
                .expect_err("no key, no relaunch");
            assert_eq!(
                refused.code, "agent_profile_revision_unavailable",
                "{host_state_dir:?} {change:?}"
            );
        }
        let records_before = crate::store::Store::new(frozen.store_path.clone())
            .load_sessions()
            .expect("load")
            .len();
        let created = broken
            .create(frozen.new_params())
            .await
            .expect_err("a profile session cannot be frozen without the key");
        assert_eq!(created.code, "agent_profile_revision_unavailable");
        assert_eq!(
            crate::store::Store::new(frozen.store_path.clone())
                .load_sessions()
                .expect("load")
                .len(),
            records_before,
            "the refused create persisted nothing"
        );

        let bare = broken
            .create(SessionNewParams {
                agent: "pi".to_owned(),
                cwd: Some(frozen.dir.clone()),
                ..params()
            })
            .await
            .expect("a session without a profile needs no key");
        let _ = broken.stop(&bare.id).await;
    }
    assert_eq!(
        frozen.marks(),
        [SECRET_MARK, "unset", "unset"],
        "the first profile run and one bare run per configuration launched; no profile session did"
    );
}

#[tokio::test]
async fn fork_of_an_unchanged_profile_freezes_the_same_revision() {
    let frozen = Frozen::new("fork-same");
    let registry = frozen.registry();
    let id = frozen.created_and_exited(&registry).await;
    let revision = frozen.frozen_revision(&id);
    drop(registry);

    let restarted = frozen.restarted().await;
    let forked = restarted.fork(fork_params(&id, false)).await.expect("fork");

    assert_eq!(
        frozen.wait_for_launches(2).await,
        [SECRET_MARK, SECRET_MARK]
    );
    assert_eq!(frozen.frozen_revision(&forked.id), revision);
    let _ = restarted.stop(&forked.id).await;
}

#[tokio::test]
async fn fork_of_an_edited_or_deleted_profile_is_refused_and_the_override_forks_under_the_current_one(
) {
    let frozen = Frozen::new("fork-edit");
    let registry = frozen.registry();
    let id = frozen.created_and_exited(&registry).await;
    drop(registry);

    frozen.write_profile("second-account");
    let restarted = frozen.restarted().await;
    let refused = restarted
        .fork(fork_params(&id, false))
        .await
        .expect_err("an edited profile is not forked under silently");
    assert_eq!(refused.code, "agent_profile_changed");
    assert_eq!(launch_count(&frozen.marker), 1);
    assert_eq!(
        restarted.list().await.len(),
        1,
        "a refused fork registers no session"
    );

    let forked = restarted
        .fork(fork_params(&id, true))
        .await
        .expect("the owner accepts the edited profile");
    assert_eq!(
        frozen.wait_for_launches(2).await,
        [SECRET_MARK, "second-account"]
    );
    assert_eq!(
        frozen.frozen_revision(&forked.id),
        Some(current_revision(&restarted)),
        "the fork freezes the revision it launched under"
    );
    assert_ne!(
        frozen.frozen_revision(&forked.id),
        frozen.frozen_revision(&id),
        "the source keeps its own revision"
    );
    let _ = restarted.stop(&forked.id).await;

    frozen.delete_profile();
    for accept in [false, true] {
        let missing = restarted
            .fork(fork_params(&id, accept))
            .await
            .expect_err("a deleted profile cannot be forked under");
        assert_eq!(missing.code, "agent_profile_missing", "accept={accept}");
    }
}

#[tokio::test]
async fn resume_resolves_the_profile_once_so_an_edit_during_the_operation_cannot_launch() {
    let frozen = Frozen::new("resume-once");
    let registry = frozen.registry();
    let id = frozen.created_and_exited(&registry).await;
    let before = frozen.frozen_revision(&id).expect("frozen");
    let restarted = frozen.restarted().await;

    let gate = hold_recovery(&restarted);
    let task = tokio::spawn({
        let registry = restarted.clone();
        let id = id.clone();
        async move { registry.resume(&id).await }
    });
    gate.entered.notified().await;
    frozen.write_profile("edited-in-flight");
    gate.release.notify_one();
    task.await
        .expect("resume task")
        .expect("the resolved profile is what launches");

    assert_eq!(
        frozen.wait_for_launches(2).await,
        [SECRET_MARK, SECRET_MARK],
        "the child ran under the profile the operation resolved, not the edit"
    );
    assert_eq!(
        frozen.frozen_revision(&id),
        Some(before),
        "the frozen revision names the profile that launched"
    );
    restarted.stop(&id).await.expect("stop");
    let refused = restarted
        .resume(&id)
        .await
        .expect_err("the in-flight edit is caught by the next resume");
    assert_eq!(refused.code, "agent_profile_changed");
}

#[tokio::test]
async fn fork_resolves_the_profile_once_so_an_edit_during_the_operation_cannot_launch() {
    let frozen = Frozen::new("fork-once");
    let registry = frozen.registry();
    let id = frozen.created_and_exited(&registry).await;
    let restarted = frozen.restarted().await;

    let gate = hold_recovery(&restarted);
    let task = tokio::spawn({
        let registry = restarted.clone();
        let id = id.clone();
        async move { registry.fork(fork_params(&id, false)).await }
    });
    gate.entered.notified().await;
    frozen.write_profile("edited-in-flight");
    gate.release.notify_one();
    let forked = task
        .await
        .expect("fork task")
        .expect("the resolved profile is what launches");

    assert_eq!(
        frozen.wait_for_launches(2).await,
        [SECRET_MARK, SECRET_MARK]
    );
    assert_eq!(
        frozen.frozen_revision(&forked.id),
        frozen.frozen_revision(&id)
    );
    let _ = restarted.stop(&forked.id).await;
}

fn daemon_state(registry: &SessionRegistry, transport: ControlTransport) -> DaemonState {
    DaemonState::new(
        HealthInfo::new("test"),
        registry.clone(),
        Arc::new(crate::governance::HostGovernanceService::open_test()),
        crate::test_support::overlay_registry(),
    )
    .with_transport(transport)
}

/// Sends `method` with `params` through the control dispatcher.
async fn call(
    state: &DaemonState,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, ProtocolError> {
    let request = Request::new("profile-freeze", method, params).expect("request");
    handle_request(&request, state).await.into_result()
}

fn resume_with_override(id: &SessionId) -> serde_json::Value {
    serde_json::json!({"session_id": id, "accept_profile_change": true})
}

fn fork_with_override(id: &SessionId) -> serde_json::Value {
    serde_json::json!({"session_id": id, "cols": 80, "rows": 24, "accept_profile_change": true})
}

#[tokio::test]
async fn a_remote_connection_cannot_accept_a_changed_profile() {
    let frozen = Frozen::new("remote-override");
    let registry = frozen.registry();
    let id = frozen.created_and_exited(&registry).await;
    drop(registry);
    frozen.write_profile("second-account");
    let restarted = frozen.restarted().await;
    let remote = daemon_state(&restarted, ControlTransport::Remote);
    let before = frozen.frozen_revision(&id);

    for (name, params) in [
        (method::SESSION_RESUME, resume_with_override(&id)),
        (method::SESSION_FORK, fork_with_override(&id)),
    ] {
        let refused = call(&remote, name, params)
            .await
            .expect_err("a remote caller cannot hold the owner's decision");
        assert_eq!(refused.code, "agent_profile_change_local_only", "{name}");
    }
    assert_eq!(launch_count(&frozen.marker), 1, "nothing launched");
    assert_eq!(frozen.frozen_revision(&id), before);
    assert_eq!(restarted.list().await.len(), 1, "no fork registered");

    // Without the override a remote caller reaches the same typed refusal a
    // local one gets.
    let plain = call(&remote, method::SESSION_RESUME, serde_json::json!(id))
        .await
        .expect_err("an edited profile is refused");
    assert_eq!(plain.code, "agent_profile_changed");
}

#[tokio::test]
async fn a_local_connection_accepts_a_changed_profile_and_the_bare_id_shape_still_resumes() {
    let frozen = Frozen::new("local-override");
    let registry = frozen.registry();
    let id = frozen.created_and_exited(&registry).await;
    drop(registry);
    frozen.write_profile("second-account");
    let restarted = frozen.restarted().await;
    let local = daemon_state(&restarted, ControlTransport::Local);

    let bare = call(&local, method::SESSION_RESUME, serde_json::json!(id))
        .await
        .expect_err("the bare id shape is the refusing shape");
    assert_eq!(bare.code, "agent_profile_changed");

    call(&local, method::SESSION_RESUME, resume_with_override(&id))
        .await
        .expect("the local owner accepts the edit");
    assert_eq!(
        frozen.wait_for_launches(2).await,
        [SECRET_MARK, "second-account"]
    );

    restarted.stop(&id).await.expect("stop");
    call(&local, method::SESSION_RESUME, serde_json::json!(id))
        .await
        .expect("the old bare-id request shape resumes the re-frozen profile");
    assert_eq!(
        frozen.wait_for_launches(3).await,
        [SECRET_MARK, "second-account", "second-account"]
    );

    let forked = call(&local, method::SESSION_FORK, fork_with_override(&id))
        .await
        .expect("a local fork with the override");
    assert!(forked["id"].is_string(), "{forked}");
    let _ = restarted.stop(&id).await;
}

#[tokio::test]
async fn a_profile_that_shadowed_a_runtime_id_is_missing_once_it_is_deleted() {
    // The profile `pi` shadows the runtime `pi`: deleting the file leaves a
    // name that still resolves, to the bare runtime, which is not the profile
    // the session was frozen under.
    let frozen = Frozen::named("shadow-deleted", "pi");
    let registry = frozen.registry();
    let id = frozen.created_and_exited(&registry).await;
    assert!(frozen.frozen_revision(&id).is_some());
    drop(registry);

    frozen.delete_profile();
    let restarted = frozen.restarted().await;
    for change in [ProfileChange::Refuse, ProfileChange::Accept] {
        let refused = restarted
            .resume_with(&id, change)
            .await
            .expect_err("the bare runtime is not the frozen profile");
        assert_eq!(refused.code, "agent_profile_missing", "{change:?}");
    }
    assert_eq!(launch_count(&frozen.marker), 1);
}

#[tokio::test]
async fn a_profile_added_over_the_name_of_a_bare_session_needs_the_override() {
    let frozen = Frozen::named("shadow-added", "pi");
    frozen.delete_profile();
    let registry = frozen.registry();
    let created = registry
        .create(frozen.new_params())
        .await
        .expect("create a bare runtime session");
    wait_for_file_contains(&frozen.marker, "--session-id").await;
    release(&frozen.gate);
    registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("session exits");
    assert_eq!(frozen.frozen_revision(&created.id), None);

    // The same name now resolves to a host profile with its own environment.
    frozen.write_profile("late-profile");
    let refused = registry
        .resume(&created.id)
        .await
        .expect_err("a profile that appeared since launch is not applied silently");
    assert_eq!(refused.code, "agent_profile_changed");
    assert_eq!(launch_count(&frozen.marker), 1);

    registry
        .resume_with(&created.id, ProfileChange::Accept)
        .await
        .expect("the owner accepts the new profile");
    assert_eq!(frozen.wait_for_launches(2).await, ["unset", "late-profile"]);
    let _ = registry.stop(&created.id).await;
}
