//! Durable worker lifecycle against the real native supervisor of the target.
//!
//! Linux drives the systemd user manager (transient worker units, the
//! namespaced daemon unit); these tests are ignored unless
//! `POHUNEK_SYSTEMD_E2E=1` opts into the real manager. macOS drives launchd's
//! `gui/<uid>` domain; there they run in the ordinary `cargo test` and fail,
//! never skip, when the domain is missing.
//!
//! Every test owns an isolated installation (see `supervised/mod.rs`). The
//! daemon runs as its native login job through the platform
//! `DaemonSupervisor`, so graceful restarts and `SIGKILL` go through the real
//! manager's restart policy. Only the Linux manager-outage scenario runs the
//! daemon as a direct child, because it must point the daemon's D-Bus
//! connection at a relay, and a native job definition cannot carry that
//! variable.
//!
//! Upgrading with live workers is covered with the `pohunek service` engine in
//! `crates/cli/tests/service_upgrade.rs`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

// Rust guideline compliant 2026-09-24

mod supervised;

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::time::Duration;

use nix::sys::signal::Signal;
use pohunek_platform::process::ProcessIdentity;
use pohunek_platform::supervisor::{
    JobDefinition, JobSpec, Namespace, RestartPolicy, Supervisor, WorkerKey,
};
use pohunek_worker_protocol::{
    ControlCode, ControlError, ControlMessage, ControlReader, ControlResponse, ControlWriter,
    ResponseKind,
};
use protocol::{
    method, RuntimeInventoryStatus, RuntimeState, SessionId, SessionInputParams,
    SessionResizeParams,
};
use supervised::{
    backend, contains, counters, eventually, explained, loss_reason, read_until, runtime_state,
    signal, stays, Installation, Settings, Snapshot,
};

/// Reason of a proven worker crash whose marker sweep completed.
const RUNTIME_LOST: &str = "runtime_lost";

/// Reason of a record whose supervisor could not be inspected.
const SUPERVISION_UNAVAILABLE: &str = "runtime_supervision_unavailable";

/// Reason of a job, worker, or journal naming another generation or peer.
const IDENTITY_MISMATCH: &str = "runtime_identity_mismatch";

/// Reason of a worker that speaks no compatible private protocol.
const PROTOCOL_INCOMPATIBLE: &str = "worker_protocol_incompatible";

/// Inventory reason of a live job whose generation no record names.
const STALE_GENERATION: &str = "stale_worker_generation";

/// How long a settled state must hold to rule out a delayed restart or kill.
///
/// Longer than the fixture's restart throttle, sweep grace, and first
/// supervision retry together, so any automatic follow-up would show.
const SETTLED: Duration = Duration::from_secs(4);

/// Rounds of the repeated lifecycle scenario.
const CYCLE_ROUNDS: usize = 5;

/// Script of a stand-in job that keeps running until it is retired.
///
/// The shell stays the job's main process (`wait`), so launchd's process match
/// on the definition's executable holds for its whole life.
const RUNNING_JOB: &str = "sleep 600 & wait";

/// Script of a stand-in job that has already ended.
const ENDED_JOB: &str = "exit 3";

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"
)]
async fn two_workers_survive_graceful_restart_and_sigkill() {
    let fixture = Installation::new(Settings::default()).await;
    let first_daemon = fixture.start_daemon().await;
    let sessions = [
        fixture.new_session("restart-a").await.id.0,
        fixture.new_session("restart-b").await.id.0,
    ];
    let mut before = Vec::new();
    for session in &sessions {
        before.push(fixture.snapshot(session).await);
    }
    let jobs = sorted_ids(&before);
    assert_eq!(fixture.job_ids().await, jobs, "one job per session");

    let restarted = fixture.restart_daemon().await;
    assert_ne!(restarted, first_daemon);
    for snapshot in &before {
        fixture.assert_same_runtime(snapshot).await;
    }

    let watched = &sessions[0];
    let at_kill = fixture.counter(watched);
    let (killed, revived) = fixture.kill_daemon().await;
    let at_ready = fixture.counter(watched);
    assert_eq!(killed, restarted);
    assert_ne!(revived, killed);
    assert!(
        at_ready > at_kill,
        "the agent kept producing output while the daemon was absent ({at_kill} -> {at_ready})"
    );
    for snapshot in &before {
        fixture.assert_same_runtime(snapshot).await;
    }
    let mut attach = fixture.attach(watched).await;
    let output = read_until(&mut attach, "output from the daemon's absence", |bytes| {
        counters(bytes)
            .iter()
            .any(|value| *value > at_kill && *value <= at_ready)
    })
    .await;
    assert!(
        contains(&output, b"\x1b[2J\x1b[H"),
        "a fresh attach repaints the terminal"
    );

    let mut client = fixture.client().await;
    for (index, session) in sessions.iter().enumerate() {
        let cols = 100 + u16::try_from(index).expect("small index");
        client
            .call::<method::SessionResize>(SessionResizeParams {
                session_id: SessionId(session.clone()),
                cols,
                rows: 30,
            })
            .await
            .expect("resize after the daemon returned");
        let text = format!("after-sigkill-{index}");
        let mut attach = fixture.attach(session).await;
        client
            .call::<method::SessionInput>(SessionInputParams {
                session_id: SessionId(session.clone()),
                text: text.clone(),
                wait: None,
            })
            .await
            .expect("input after the daemon returned");
        let marker = format!("input:{text}");
        read_until(&mut attach, &marker, |bytes| {
            contains(bytes, marker.as_bytes())
        })
        .await;
        let info = fixture.inspect(session).await;
        assert_eq!((info.cols, info.rows), (cols, 30));
    }
    for snapshot in &before {
        fixture.assert_same_runtime(snapshot).await;
    }
    assert_eq!(fixture.job_ids().await, jobs, "no duplicate generation");
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"
)]
async fn crashed_worker_is_lost_swept_and_never_restarted() {
    let fixture = Installation::new(Settings::default()).await;
    fixture.start_daemon().await;
    let survivor = fixture.new_session("survivor").await.id.0;
    let crashed = fixture.new_session("crashed").await.id.0;
    let survivor_before = fixture.snapshot(&survivor).await;
    let crashed_before = fixture.snapshot(&crashed).await;
    let survivor_descendant = fixture.descendant(&survivor).await;
    let crashed_descendant = fixture.descendant(&crashed).await;

    signal(crashed_before.worker.pid, Signal::SIGKILL);
    fixture
        .wait_reason(&crashed, RuntimeState::Lost, RUNTIME_LOST)
        .await;

    // A complete sweep is what `runtime_lost` records; on Linux the unit's
    // control-group kill may have ended the descendant first.
    assert!(
        !fixture.is_running(crashed_descendant),
        "the crashed generation's hangup-ignoring descendant was reaped"
    );
    assert!(
        fixture.is_running(survivor_descendant),
        "the other generation's descendant is untouched"
    );
    fixture.assert_same_runtime(&survivor_before).await;
    assert!(fixture.job_absent(&crashed_before.service_id()).await);
    let only_survivor = vec![survivor_before.service_id().to_string()];
    stays(
        "no restart or duplicate of the crashed session",
        SETTLED,
        || async {
            fixture.job_ids().await == only_survivor
                && fixture
                    .record(&crashed)
                    .and_then(|record| record.runtime.generation)
                    .as_deref()
                    == Some(crashed_before.generation.as_str())
                && runtime_state(&fixture.inspect(&crashed).await) == Some(RuntimeState::Lost)
        },
    )
    .await;
    fixture.assert_same_runtime(&survivor_before).await;
    assert!(fixture.is_running(survivor_descendant));
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"
)]
async fn late_worker_is_retired_and_every_path_converges_to_one_generation() {
    let settings = Settings {
        worker_connect: Duration::from_secs(2),
        worker_initialize: Duration::from_secs(8),
        ..Settings::default()
    };
    let fixture = Installation::new(settings).await;
    let held = HeldWorker::install(&fixture);
    fixture.start_daemon().await;

    // Registration succeeds, readiness never comes: the create fails and
    // exactly the late generation is retired.
    let pending = spawn_create(&fixture, "late").await;
    let late = new_job(&fixture, &[]).await;
    let failure = pending
        .await
        .expect("create task")
        .expect_err("a worker that never becomes ready fails the create");
    eprintln!("late create failed with {failure:?}");
    wait_converged(&fixture, "late", &[]).await;

    // The client abandons its request after registration: the daemon still
    // owns the outcome and retires the late generation.
    let pending = spawn_create(&fixture, "cancelled").await;
    let cancelled = new_job(&fixture, &[]).await;
    pending.abort();
    wait_converged(&fixture, "cancelled", &[]).await;

    // The daemon restarts while a registration is pending: the abandoned
    // generation is settled instead of being left behind.
    let pending = spawn_create(&fixture, "restarted").await;
    let restarted = new_job(&fixture, &[]).await;
    fixture.restart_daemon().await;
    let _abandoned = pending.await;
    wait_converged(&fixture, "restarted", &[]).await;

    // A retry with a worker that becomes ready yields exactly one generation.
    held.release();
    let retry = fixture.new_session("retry").await.id.0;
    let retry_before = fixture.snapshot(&retry).await;
    let only_retry = vec![retry_before.service_id().to_string()];
    stays("exactly one generation", SETTLED, || async {
        fixture.job_ids().await == only_retry
    })
    .await;
    fixture.assert_same_runtime(&retry_before).await;
    for retired in [&late, &cancelled, &restarted] {
        assert!(!only_retry.contains(retired));
    }
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"
)]
async fn foreign_malformed_stale_and_incompatible_inputs_fail_closed() {
    let mut fixture = Installation::new(Settings::default()).await;
    let namespace = fixture.namespace();

    // A live and an ended job of generations no record names.
    let stale_running = WorkerKey::new("s-4101", "stal2345").expect("key");
    let stale_ended = WorkerKey::new("s-4102", "stal3456").expect("key");
    let stale_process =
        start_running(&fixture, &*fixture.workers, &namespace, &stale_running).await;
    start_job(
        &fixture,
        &*fixture.workers,
        &namespace,
        &stale_ended,
        ENDED_JOB,
    )
    .await;

    // A well-formed job of another installation sharing the manager.
    let foreign_namespace = Namespace::derive(
        supervised::uid(),
        &fixture.root.join("foreign-state"),
        &fixture.root.join("foreign-runtime"),
    );
    let foreign_key = WorkerKey::new("s-4103", "frgn2345").expect("key");
    let foreign = backend::foreign_workers(&fixture.root, &foreign_namespace).await;
    fixture.extra().job(&foreign_namespace, &foreign_key);
    let foreign_process =
        start_running(&fixture, &*foreign, &foreign_namespace, &foreign_key).await;

    // A job in the namespace whose name carries no session or generation.
    let paths = fixture.paths.clone();
    let malformed = backend::plant_malformed(fixture.extra(), &paths, &namespace);

    // A record whose current generation is served by an incompatible peer.
    let incompatible = WorkerKey::new("s-4104", "inco2345").expect("key");
    let incompatible_process =
        start_running(&fixture, &*fixture.workers, &namespace, &incompatible).await;
    let mut runtime = fixture.runtime_record(&incompatible, RuntimeState::Live);
    runtime.worker_id = Some("w-incompatible".to_owned());
    fixture.seed_record(incompatible.session_id(), runtime);
    let peer = IncompatiblePeer::bind(&fixture, incompatible.session_id());

    fixture.start_daemon().await;

    fixture
        .wait_reason(
            incompatible.session_id(),
            RuntimeState::Incompatible,
            PROTOCOL_INCOMPATIBLE,
        )
        .await;
    eventually("retirement of the ended stale job", || async {
        fixture
            .job_absent(&stale_ended.service_id())
            .await
            .then_some(())
    })
    .await;
    if let Some(labels) = backend::definition_labels(&fixture.paths) {
        assert!(
            !labels.contains(&namespace.worker_label(&stale_ended)),
            "the ended stale job's definition was removed: {labels:?}"
        );
        assert!(labels.contains(&namespace.worker_label(&stale_running)));
        assert!(labels.contains(&malformed), "unowned definitions are kept");
    }
    let inventory = fixture.inventory().await;
    assert!(
        inventory.entries.iter().any(|entry| {
            entry.runtime_slot == stale_running.service_id().as_str()
                && entry.status == RuntimeInventoryStatus::Orphaned
                && entry.reason.as_deref() == Some(STALE_GENERATION)
        }),
        "the live stale job is reported, not killed: {inventory:?}"
    );
    stays(
        "nothing but the ended stale job was touched",
        SETTLED,
        || async {
            fixture.is_running(stale_process)
                && fixture.is_running(foreign_process)
                && fixture.is_running(incompatible_process)
                && backend::planted_alive(&malformed)
                && peer.alive()
        },
    )
    .await;
    let info = fixture.inspect(incompatible.session_id()).await;
    assert_eq!(runtime_state(&info), Some(RuntimeState::Incompatible));
    assert_eq!(loss_reason(&info), Some(PROTOCOL_INCOMPATIBLE));
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"
)]
async fn mismatched_evidence_fails_closed_and_a_reused_pid_counts_as_ended() {
    let fixture = Installation::new(Settings::default()).await;
    fixture.start_daemon().await;
    let peer = fixture.new_session("peer").await.id.0;
    let journal = fixture.new_session("journal").await.id.0;
    let definition = fixture.new_session("definition").await.id.0;
    let reused = fixture.new_session("reused").await.id.0;
    let mut kept = Vec::new();
    for session in [&peer, &journal, &definition] {
        kept.push((
            fixture.snapshot(session).await,
            fixture.descendant(session).await,
        ));
    }
    let reused_before = fixture.snapshot(&reused).await;
    let reused_descendant = fixture.descendant(&reused).await;
    fixture.stop_daemon().await;

    // The record expects another peer than the worker serving the socket.
    fixture.patch_record(&peer, |record| {
        record.runtime.worker_id = Some(format!("{}x", kept[0].0.worker_id));
    });
    // The live worker's journal names another generation than the record.
    fixture.patch_journal(&journal, &kept[1].0.worker_id, |value| {
        value["generation"] = serde_json::Value::from("zzzz2345");
    });
    // The record names another executable than the job's definition runs.
    let foreign_executable = fixture
        .root
        .join("prefix/libexec/pohunek/other/pohunek-sessiond");
    fixture.patch_record(&definition, |record| {
        record.runtime.executable = Some(foreign_executable.clone());
    });
    // The worker crashes while the daemon is away and its journaled PID is
    // reused by an unrelated process.
    signal(reused_before.worker.pid, Signal::SIGKILL);
    fixture
        .wait_gone(reused_before.worker, "crashed worker")
        .await;
    let decoy = Decoy::spawn();
    let decoy_process = fixture.identity(decoy.pid()).expect("decoy runs");
    fixture.patch_journal(&reused, &reused_before.worker_id, |value| {
        value["worker_pid"] = serde_json::Value::from(decoy.pid());
    });

    fixture.start_daemon().await;

    for session in [&peer, &journal, &definition] {
        fixture
            .wait_reason(session, RuntimeState::Conflict, IDENTITY_MISMATCH)
            .await;
    }
    fixture
        .wait_reason(&reused, RuntimeState::Lost, RUNTIME_LOST)
        .await;
    assert!(
        fixture.is_running(decoy_process),
        "the process reusing the PID was not signalled"
    );
    assert!(!fixture.is_running(reused_descendant));
    assert!(fixture.job_absent(&reused_before.service_id()).await);
    stays("mismatched generations are left alive", SETTLED, || async {
        for (snapshot, descendant) in &kept {
            if !fixture.is_running(snapshot.worker)
                || !fixture.is_running(snapshot.child)
                || !fixture.is_running(*descendant)
                || fixture.job_absent(&snapshot.service_id()).await
            {
                return false;
            }
        }
        true
    })
    .await;
}

/// An unrelated process that takes over a crashed worker's journaled PID.
struct Decoy(std::process::Child);

impl Decoy {
    fn spawn() -> Self {
        Self(
            std::process::Command::new("sleep")
                .arg("600")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn the PID-reuse decoy"),
        )
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for Decoy {
    fn drop(&mut self) {
        // The decoy is this test's own child; nothing else may reap it.
        let _killed = self.0.kill();
        let _reaped = self.0.wait();
    }
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
async fn unavailable_manager_kills_nothing_until_it_answers_again() {
    let settings = Settings {
        manager_command: Duration::from_secs(2),
        ..Settings::default()
    };
    let fixture = Installation::new(settings).await;
    let relay = backend::BusRelay::start(fixture.root.join("bus"));
    let _daemon = fixture
        .spawn_daemon(&[("DBUS_SESSION_BUS_ADDRESS", relay.address())])
        .await;
    let (survivor, affected) = two_sessions(&fixture).await;

    relay.pause();
    signal(affected.0.worker.pid, Signal::SIGKILL);
    fixture
        .wait_reason(
            &affected.0.session_id,
            RuntimeState::Reconnecting,
            SUPERVISION_UNAVAILABLE,
        )
        .await;
    // systemd keeps the failed transient unit loaded until someone resets
    // it; the daemon cannot while its manager is unreachable.
    assert!(!fixture.job_absent(&affected.0.service_id()).await);
    stays(
        "an unreachable manager resolves nothing",
        SETTLED,
        || async {
            loss_reason(&fixture.inspect(&affected.0.session_id).await)
                == Some(SUPERVISION_UNAVAILABLE)
                && fixture.is_running(survivor.1)
        },
    )
    .await;
    fixture.assert_same_runtime(&survivor.0).await;

    relay.resume();
    assert_recovered_after_outage(&fixture, &survivor, &affected).await;
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn unavailable_manager_kills_nothing_until_it_answers_again() {
    /// A `launchctl` deadline no invocation can meet.
    const UNREACHABLE_COMMAND: Duration = Duration::from_millis(1);

    let mut fixture = Installation::new(Settings::default()).await;
    fixture.start_daemon().await;
    let (survivor, affected) = two_sessions(&fixture).await;
    fixture.stop_daemon().await;
    signal(affected.0.worker.pid, Signal::SIGKILL);
    fixture.wait_gone(affected.0.worker, "crashed worker").await;
    let working = fixture.config.deadlines().launchctl_command;
    fixture.rewrite_config(|spec| spec.deadlines.launchctl_command = UNREACHABLE_COMMAND);
    fixture.start_daemon().await;

    fixture
        .wait_reason(
            &affected.0.session_id,
            RuntimeState::Reconnecting,
            SUPERVISION_UNAVAILABLE,
        )
        .await;
    let label = fixture.namespace().worker_label(
        &WorkerKey::new(affected.0.session_id.clone(), affected.0.generation.clone()).expect("key"),
    );
    stays(
        "an unreachable manager resolves nothing",
        SETTLED,
        || async {
            loss_reason(&fixture.inspect(&affected.0.session_id).await)
                == Some(SUPERVISION_UNAVAILABLE)
                && fixture.is_running(affected.1)
                && fixture.is_running(survivor.1)
                && backend::definition_labels(&fixture.paths)
                    .is_some_and(|labels| labels.contains(&label))
        },
    )
    .await;
    fixture.assert_same_runtime(&survivor.0).await;

    fixture.stop_daemon().await;
    fixture.rewrite_config(|spec| spec.deadlines.launchctl_command = working);
    fixture.start_daemon().await;
    assert_recovered_after_outage(&fixture, &survivor, &affected).await;
    assert!(
        backend::definition_labels(&fixture.paths).is_some_and(|labels| !labels.contains(&label))
    );
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"
)]
async fn repeated_start_stop_recover_cycles_leave_no_orphans() {
    let fixture = Installation::new(Settings::default()).await;
    let recovered = "s-5000";
    fixture.seed_record(
        recovered,
        pohunek_daemon::store::RuntimeRecord {
            state: RuntimeState::Lost,
            worker_id: None,
            runtime_id: None,
            service_id: None,
            generation: None,
            executable: None,
            reason: Some("worker_unavailable".to_owned()),
        },
    );
    fixture.start_daemon().await;
    let namespace = fixture.namespace();
    let mut generations = BTreeSet::new();
    // A stopped session keeps its current generation's job, which holds the
    // final output; every other generation must be gone after each round.
    let mut kept = BTreeMap::<String, Snapshot>::new();
    let mut stopped_sessions = Vec::new();
    for round in 0..CYCLE_ROUNDS {
        let stopped = fixture.new_session(&format!("stopped-{round}")).await.id.0;
        let removed = fixture.new_session(&format!("removed-{round}")).await.id.0;
        let previous = kept.remove(recovered);
        let resumed = fixture
            .client()
            .await
            .call::<method::SessionResume>(SessionId(recovered.to_owned()))
            .await
            .unwrap_or_else(|error| panic!("recover round {round}: {error}"))
            .session;
        assert_eq!(resumed.id.0, recovered);
        let snapshots = [
            fixture.snapshot(&stopped).await,
            fixture.snapshot(&removed).await,
            fixture.snapshot(recovered).await,
        ];
        assert!(
            generations.insert(snapshots[2].generation.clone()),
            "every recovery mints a new generation"
        );
        if let Some(previous) = previous {
            fixture
                .wait_gone(previous.worker, "superseded recovered worker")
                .await;
        }

        let mut client = fixture.client().await;
        for session in [&stopped, recovered] {
            client
                .call::<method::SessionStop>(SessionId(session.to_owned()))
                .await
                .unwrap_or_else(|error| panic!("stop {session}: {error}"));
        }
        client
            .call::<method::SessionRemove>(SessionId(removed.clone()))
            .await
            .unwrap_or_else(|error| panic!("remove {removed}: {error}"));
        if round % 2 == 0 {
            fixture.restart_daemon().await;
        } else {
            fixture.kill_daemon().await;
        }
        kept.insert(stopped.clone(), snapshots[0].clone());
        kept.insert(recovered.to_owned(), snapshots[2].clone());
        stopped_sessions.push(stopped);
        fixture
            .wait_gone(snapshots[1].worker, "removed session's worker")
            .await;
        assert_only_current_jobs(&fixture, &namespace, kept.values(), round).await;
    }
    stays("no orphan job reappears", SETTLED, || async {
        fixture.job_ids().await == sorted_ids(&kept.values().cloned().collect::<Vec<_>>())
    })
    .await;
    assert_eq!(generations.len(), CYCLE_ROUNDS);

    let mut client = fixture.client().await;
    for session in stopped_sessions
        .iter()
        .map(String::as_str)
        .chain([recovered])
    {
        client
            .call::<method::SessionRemove>(SessionId(session.to_owned()))
            .await
            .unwrap_or_else(|error| panic!("remove {session}: {error}"));
    }
    assert_only_current_jobs(&fixture, &namespace, std::iter::empty(), CYCLE_ROUNDS).await;
    for snapshot in kept.values() {
        fixture
            .wait_gone(snapshot.worker, "removed session's worker")
            .await;
    }
}

/// Waits until the namespace holds exactly the jobs (and, on launchd, the
/// definitions) of `current`.
async fn assert_only_current_jobs(
    fixture: &Installation,
    namespace: &Namespace,
    current: impl Iterator<Item = &Snapshot>,
    round: usize,
) {
    let current = current.cloned().collect::<Vec<_>>();
    let expected = sorted_ids(&current);
    explained(
        &format!("only current generations after round {round}"),
        || async {
            let ids = fixture.job_ids().await;
            if ids == expected {
                Ok(())
            } else {
                Err(format!("jobs {ids:?}, expected {expected:?}"))
            }
        },
    )
    .await;
    if let Some(labels) = backend::definition_labels(&fixture.paths) {
        let mut wanted = current
            .iter()
            .map(|snapshot| {
                namespace.worker_label(
                    &WorkerKey::new(snapshot.session_id.clone(), snapshot.generation.clone())
                        .expect("key"),
                )
            })
            .collect::<Vec<_>>();
        wanted.sort();
        assert_eq!(labels, wanted, "round {round} definitions");
    }
}

/// Creates two live sessions and captures their runtimes and descendants.
async fn two_sessions(
    fixture: &Installation,
) -> ((Snapshot, ProcessIdentity), (Snapshot, ProcessIdentity)) {
    let survivor = fixture.new_session("survivor").await.id.0;
    let affected = fixture.new_session("affected").await.id.0;
    (
        (
            fixture.snapshot(&survivor).await,
            fixture.descendant(&survivor).await,
        ),
        (
            fixture.snapshot(&affected).await,
            fixture.descendant(&affected).await,
        ),
    )
}

/// Asserts the outage's crash is resolved exactly once the manager answers.
async fn assert_recovered_after_outage(
    fixture: &Installation,
    survivor: &(Snapshot, ProcessIdentity),
    affected: &(Snapshot, ProcessIdentity),
) {
    fixture
        .wait_reason(&affected.0.session_id, RuntimeState::Lost, RUNTIME_LOST)
        .await;
    assert!(
        !fixture.is_running(affected.1),
        "the lost runtime was swept"
    );
    assert!(fixture.job_absent(&affected.0.service_id()).await);
    assert!(fixture.is_running(survivor.1));
    fixture.assert_same_runtime(&survivor.0).await;
    assert_eq!(
        fixture.job_ids().await,
        vec![survivor.0.service_id().to_string()]
    );
}

/// Starts a stand-in job for `key` that keeps running and returns its process.
async fn start_running(
    fixture: &Installation,
    supervisor: &dyn Supervisor,
    namespace: &Namespace,
    key: &WorkerKey,
) -> ProcessIdentity {
    start_job(fixture, supervisor, namespace, key, RUNNING_JOB).await;
    job_process(supervisor, key).await
}

/// Waits until the job of `key` has a process and returns it.
async fn job_process(supervisor: &dyn Supervisor, key: &WorkerKey) -> ProcessIdentity {
    eventually(&format!("process of job {}", key.service_id()), || async {
        supervisor
            .inspect(&key.service_id())
            .await
            .ok()
            .and_then(|job| job.process)
    })
    .await
}

/// Sorted service IDs of `snapshots`.
fn sorted_ids(snapshots: &[Snapshot]) -> Vec<String> {
    let mut ids = snapshots
        .iter()
        .map(|snapshot| snapshot.service_id().to_string())
        .collect::<Vec<_>>();
    ids.sort();
    ids
}

/// Sends `session.new` from a task the test can await or abort.
async fn spawn_create(
    fixture: &Installation,
    name: &str,
) -> tokio::task::JoinHandle<Result<protocol::SessionNewResult, pohunek_client::ClientError>> {
    let mut client = fixture.client().await;
    let params = fixture.new_params(name);
    tokio::spawn(async move { client.call::<method::SessionNew>(params).await })
}

/// Waits for a job that is not in `known` and returns its service ID.
async fn new_job(fixture: &Installation, known: &[String]) -> String {
    eventually("a newly registered job", || async {
        fixture
            .job_ids()
            .await
            .into_iter()
            .find(|id| !known.contains(id))
    })
    .await
}

/// Waits until the namespace runs exactly `jobs` and no session named `name`
/// still claims a runtime: the abandoned generation was retired by exact
/// service ID and its create was settled.
async fn wait_converged(fixture: &Installation, name: &str, jobs: &[String]) {
    explained(&format!("convergence after {name}"), || async {
        let ids = fixture.job_ids().await;
        let claiming = fixture
            .sessions()
            .await
            .into_iter()
            .filter(|info| info.name.as_deref() == Some(name))
            .filter(|info| {
                matches!(
                    runtime_state(info),
                    Some(
                        RuntimeState::Live
                            | RuntimeState::Starting
                            | RuntimeState::Reconnecting
                            | RuntimeState::Conflict
                    )
                )
            })
            .map(|info| info.runtime)
            .collect::<Vec<_>>();
        if ids == jobs && claiming.is_empty() {
            Ok(())
        } else {
            Err(format!("jobs {ids:?}; {name} runtimes {claiming:?}"))
        }
    })
    .await;
}

/// Starts a stand-in job for `key` through `supervisor`.
///
/// The definition names the session and generation exactly as the daemon's
/// worker definitions do, so only the generation evidence tells it apart.
async fn start_job(
    fixture: &Installation,
    supervisor: &dyn Supervisor,
    namespace: &Namespace,
    key: &WorkerKey,
    script: &str,
) {
    let definition = JobDefinition::new(JobSpec {
        executable: PathBuf::from("/bin/bash"),
        arguments: vec![
            "-c".to_owned(),
            script.to_owned(),
            "pohunek-stand-in".to_owned(),
            "--session-id".to_owned(),
            key.session_id().to_owned(),
            "--worker-generation".to_owned(),
            key.generation().to_owned(),
        ],
        environment: BTreeMap::from([(
            "HOME".to_owned(),
            supervised::path_string(&fixture.home()),
        )]),
        working_directory: fixture.home(),
        logs: backend::worker_logs(&fixture.paths, namespace, key),
        start_timeout: Duration::from_secs(10),
        exit_timeout: Duration::from_secs(2),
        restart: RestartPolicy::Never,
        open_files: 1_024,
    })
    .expect("valid stand-in definition");
    supervisor
        .start(&key.service_id(), &definition)
        .await
        .expect("start the stand-in job");
}

/// A configured worker executable whose process never becomes a worker.
///
/// While held, the version directory's worker path is a script that only
/// sleeps: its job registers and runs, but no worker socket ever appears,
/// which is exactly a registration whose readiness is late. Releasing moves
/// the real worker back to the same path, so later jobs run the real binary
/// under the path their definitions name.
struct HeldWorker {
    worker: PathBuf,
    real: PathBuf,
}

impl HeldWorker {
    /// Replaces the active version's worker with the holding script.
    fn install(fixture: &Installation) -> Self {
        let worker = fixture.config.worker_executable();
        let real = worker.with_file_name("pohunek-sessiond.real");
        std::fs::rename(&worker, &real).expect("move the real worker aside");
        std::fs::write(&worker, "#!/bin/sh\nexec sleep 3600\n").expect("write the holding worker");
        std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o755))
            .expect("executable holding worker");
        Self { worker, real }
    }

    /// Puts the real worker back under the configured path.
    fn release(&self) {
        std::fs::rename(&self.real, &self.worker).expect("restore the real worker");
    }
}

/// A worker endpoint that negotiates no compatible protocol version.
///
/// It stands in for a worker of an incompatible release at the record's
/// socket, served by the test process because no such build exists.
struct IncompatiblePeer {
    socket: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl IncompatiblePeer {
    fn bind(fixture: &Installation, session_id: &str) -> Self {
        let workers = fixture
            .paths
            .runtime_dir
            .join(pohunek_paths::WORKERS_SUBDIR);
        let directory = workers.join(session_id);
        std::fs::create_dir_all(&directory).expect("create the worker directory");
        for path in [&workers, &directory] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .expect("private worker directory");
        }
        let socket = directory.join(pohunek_paths::WORKER_SOCKET_NAME);
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind the peer");
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .expect("private peer socket");
        let task = tokio::spawn(async move {
            while let Ok((stream, _address)) = listener.accept().await {
                tokio::spawn(refuse(stream));
            }
        });
        Self { socket, task }
    }

    /// Whether the endpoint still accepts connections.
    fn alive(&self) -> bool {
        !self.task.is_finished() && std::os::unix::net::UnixStream::connect(&self.socket).is_ok()
    }
}

impl Drop for IncompatiblePeer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Answers a negotiation with `worker_protocol_incompatible`.
async fn refuse(stream: tokio::net::UnixStream) {
    let (read, write) = stream.into_split();
    let mut reader = ControlReader::new(read);
    let mut writer = ControlWriter::new(write);
    let Ok(Some(ControlMessage::Request(request))) = reader.read::<ControlMessage>().await else {
        return;
    };
    let response = ControlMessage::Response(ControlResponse {
        request_id: request.request_id,
        kind: ResponseKind::Error {
            error: ControlError {
                code: ControlCode::WorkerProtocolIncompatible,
                message: "no compatible worker protocol version".to_owned(),
                retryable: false,
            },
        },
    });
    if writer.write(&response).await.is_ok() {
        let _flushed = writer.flush().await;
    }
}
