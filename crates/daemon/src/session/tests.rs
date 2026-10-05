use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use pohunek_test_support::time::{AutoAdvanceInhibitor, TIMER_TICK};
use protocol::{
    method, AgentActivity, CwdSource, DetectionRegionKind, DetectionRegionPreview, ErrorClass,
    Event, ForkCwdMode, OutputOffset, PackageId, PackageIdentity, PackageVersion,
    ProcessStartIdentity, ProjectSource, ReportSequence, Request, Response, RuntimeGeneration,
    RuntimeId, RuntimeRef, RuntimeState, SessionAttachParams, SessionDetectionParams,
    SessionForkParams, SessionId, SessionInfo, SessionNativeRecoveredEvent, SessionNewParams,
    SessionOutputParams, SessionReadFormat, SessionReadParams, SessionReadSource,
    SessionReleaseAgentParams, SessionReportAgentParams, SessionReportNativeIdParams,
    SessionRetentionPolicy, SessionRuntime, SessionRuntimeIdentity, SessionState,
    SessionWaitParams, SessionWaitReason, StateSource, SubagentInfo, SubagentLifecycle,
    SubagentRevision, TerminalWatermark, MAX_CONTROL_LINE_BYTES, MAX_REQUEST_ID_BYTES,
};

use crate::agent::host::{
    DefinitionOrigin, DefinitionParts, LaunchProgram, RuntimeDefinition, RuntimeHost,
};
use crate::agent::{InputRules, NativeSessionLaunch, SessionRefKind};
use crate::agent::{LaunchCommand, ResolvedAgent};
use crate::api::{dispatch_line, DaemonState, HealthInfo};
use crate::detect::{ActivityTransition, DetectorConfig, ManifestRegion, MatchContext};
use crate::external::{external_session_id, TranscriptIndex};
use crate::integration::{
    ENV_DAEMON_ID, ENV_FLAG, ENV_PROTOCOL_VERSION, ENV_SESSION_ID, ENV_SOCKET_PATH,
};
use crate::procwatch::readable_host::ReadableHost;
use crate::procwatch::{
    ExitWatch, OwnershipMarkers, Pid, ProcessFact, ProcessIdentity, ProcessInspector, StartIdentity,
};
use crate::project::detect::project_id;
use crate::runtime::{Worker, WorkerError};
use pohunek_test_support::wait::{guard, wait_until, HANG_GUARD};

use super::{
    native_report_is_current, preserve_durable_worker_metadata, terminalize_running_subagents,
    timestamp_now, worker_error_to_protocol, ActiveAgentReport, ExternalAssociationBlock,
    InputSubmission, RuntimeExit, RuntimeHandle, RuntimeWatchIdentity, SessionEntry,
    SessionRegistry, SessionRegistryConfig, ShellCommand, WorkerMetadataApplyOutcome,
    WorkerMetadataProgress, WorkerMetadataRetryCause, WorkerMetadataTracker,
    MAX_SESSION_NAME_BYTES, MAX_WORKER_METADATA_RETRY_DELAY, WORKER_METADATA_RETRY_WARN_INTERVAL,
};

mod native_supersede;

/// Bounds retries around intentional same-runtime snapshot races in transition tests.
const CONCURRENT_TRANSITION_RETRY_LIMIT: usize = 32;

/// Overall input deadline for tests that assert event ordering rather than
/// deadline expiry. It only bounds a stalled test: a starved host must not
/// expire it while a deliberately delayed worker ACK is still pending.
const ORDERING_TEST_INPUT_DEADLINE: Duration = Duration::from_secs(5);

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
static NATIVE_REPORT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn native_report_expiry() -> String {
    (OffsetDateTime::now_utc() + time::Duration::seconds(30))
        .format(&Rfc3339)
        .expect("format native report expiry")
}

async fn native_report_params(
    registry: &SessionRegistry,
    session_id: SessionId,
    agent: String,
    native_session_id: String,
    transcript_path: Option<String>,
) -> SessionReportNativeIdParams {
    let worker = {
        let sessions = registry.inner.sessions.lock().await;
        sessions
            .get(&session_id)
            .and_then(|entry| match &entry.runtime {
                RuntimeHandle::Worker(worker) => Some(worker.clone()),
                RuntimeHandle::Unavailable(_) => None,
            })
    };
    let identity = if let Some(worker) = worker {
        worker
            .inspect()
            .await
            .ok()
            .and_then(|snapshot| Some((snapshot.worker_instance_id?, snapshot.child_process?)))
    } else {
        None
    };
    let (worker_instance_id, pid, pid_start_identity) = identity.map_or_else(
        || ("runtime-unavailable".to_owned(), 1, 1),
        |(worker_instance_id, process)| {
            (
                worker_instance_id.to_string(),
                process.pid,
                process.start_identity,
            )
        },
    );
    SessionReportNativeIdParams::new(
        session_id,
        worker_instance_id,
        agent,
        pid,
        ProcessStartIdentity::new(pid_start_identity),
        ReportSequence::new(NATIVE_REPORT_SEQUENCE.fetch_add(1, Ordering::Relaxed)),
        native_report_expiry(),
        native_session_id,
        transcript_path,
    )
    .expect("valid native identity report")
}

macro_rules! native_report {
    (
        $registry:expr;
        session_id: $session_id:expr,
        worker_instance_id: $worker_instance_id:expr,
        agent: $agent:expr,
        pid: $pid:expr,
        pid_start_identity: $pid_start_identity:expr,
        sequence: $sequence:expr,
        expires_at: $expires_at:expr,
        native_session_id: $native_session_id:expr,
        transcript_path: $transcript_path:expr $(,)?
    ) => {{
        let _ = (
            $worker_instance_id,
            $pid,
            $pid_start_identity,
            $sequence,
            $expires_at,
        );
        native_report_params(
            $registry,
            $session_id,
            $agent,
            $native_session_id,
            $transcript_path,
        )
        .await
    }};
    (
        $registry:expr;
        session_id: $session_id:expr,
        agent: $agent:expr,
        native_session_id: $native_session_id:expr,
        transcript_path: $transcript_path:expr $(,)?
    ) => {
        native_report_params(
            $registry,
            $session_id,
            $agent,
            $native_session_id,
            $transcript_path,
        )
        .await
    };
}

/// Interactive shell for registries whose sessions run the default shell.
///
/// A fixed `/bin/sh` keeps the sessions independent of the host user's
/// `$SHELL` and its startup files, whose background helpers can hold the PTY
/// open past the stop deadline.
pub(super) fn hermetic_shell() -> ShellCommand {
    ShellCommand::new("/bin/sh", std::iter::empty::<String>())
}

pub(super) fn params() -> SessionNewParams {
    SessionNewParams {
        name: None,
        agent: "shell".to_owned(),
        cwd: Some(crate::test_support::thread_scoped_dir("pohunek-cwd-")),
        cols: 80,
        rows: 24,
        project: None,
        repo: None,
        branch: None,
        base_branch: None,
        input: None,
        metadata: BTreeMap::new(),
    }
}

fn running_subagent(id: &str, revision: u64) -> SubagentInfo {
    SubagentInfo {
        id: id.to_owned(),
        provider: RuntimeRef::codex(),
        parent_id: None,
        agent_type: Some("worker".to_owned()),
        lifecycle: SubagentLifecycle::Running,
        activity: Some(AgentActivity::Working),
        revision: SubagentRevision::new(revision),
        started_at_ms: 1,
        updated_at_ms: 1,
        finished_at_ms: None,
    }
}

#[test]
fn terminalizing_running_subagents_restores_public_order() {
    let mut older_running = running_subagent("older-running", 7);
    older_running.started_at_ms = 10;
    let mut newer_completed = running_subagent("newer-completed", 8);
    newer_completed.lifecycle = SubagentLifecycle::Completed;
    newer_completed.activity = None;
    newer_completed.started_at_ms = 20;
    newer_completed.finished_at_ms = Some(25);
    let mut subagents = vec![older_running, newer_completed];

    terminalize_running_subagents(&mut subagents, 30);

    assert_eq!(subagents[0].id, "newer-completed");
    assert_eq!(subagents[1].id, "older-running");
    assert_eq!(subagents[1].lifecycle, SubagentLifecycle::Lost);
}

fn production_supervisor() -> Arc<dyn crate::runtime::WorkerLauncher> {
    Arc::new(crate::runtime::SubprocessWorkerLauncher::new())
}

fn production_supervision() -> crate::runtime::SupervisionConfig {
    crate::runtime::SubprocessWorkerEnvironment {
        runtime_home: PathBuf::from("/run/user/1000"),
        state_home: PathBuf::from("/home/user/.local/state"),
        data_home: PathBuf::from("/home/user/.local/share"),
        config_home: PathBuf::from("/home/user/.config"),
        cache_home: PathBuf::from("/home/user/.cache"),
        home: PathBuf::from("/home/user"),
        daemon_socket: PathBuf::from("/run/user/1000/pohunek/daemon.sock"),
    }
    .supervision(PathBuf::from(
        "/home/user/.local/libexec/pohunek/1.0.0/pohunek-sessiond",
    ))
    .with_environment_source(crate::test_support::thread_environment_source())
}

#[test]
fn production_registry_rejects_missing_durable_worker_backend() {
    let error =
        SessionRegistry::new_production(SessionRegistryConfig::default(), production_supervisor())
            .expect_err("production registry must fail closed without worker runtime root");
    assert_eq!(error.code, "worker_backend_required");

    let unsupervised = SessionRegistryConfig {
        shell_command: hermetic_shell(),
        worker_runtime_root: Some(PathBuf::from("/run/user/1000/pohunek/workers")),
        worker_state_root: Some(PathBuf::from("/home/user/.local/state/pohunek/workers")),
        ..SessionRegistryConfig::default()
    };
    let error = SessionRegistry::new_production(unsupervised, production_supervisor())
        .expect_err("production registry must fail closed without supervision");
    assert_eq!(error.code, "worker_backend_required");

    let configured = SessionRegistryConfig {
        shell_command: hermetic_shell(),
        worker_runtime_root: Some(PathBuf::from("/run/user/1000/pohunek/workers")),
        worker_state_root: Some(PathBuf::from("/home/user/.local/state/pohunek/workers")),
        supervision: Some(production_supervision()),
        ..SessionRegistryConfig::default()
    };
    SessionRegistry::new_production(configured, production_supervisor())
        .expect("configured production registry");
}

#[test]
fn registry_reports_the_active_supervision() {
    let supervision = production_supervision();
    let expected = supervision.worker_executable.clone();
    let supervised = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        supervision: Some(supervision),
        ..SessionRegistryConfig::default()
    });

    let active = supervised.active_supervision().expect("supervision");

    assert_eq!(active.worker_executable, expected);
    // `production_supervision` carries no `--service-config`: direct children.
    assert!(!active.native);
}

#[test]
fn production_registry_rejects_invalid_observation_limits() {
    let config = SessionRegistryConfig {
        shell_command: hermetic_shell(),
        worker_runtime_root: Some(PathBuf::from("/run/user/1000/pohunek/workers")),
        worker_state_root: Some(PathBuf::from("/home/user/.local/state/pohunek/workers")),
        supervision: Some(production_supervision()),
        observation_output_bytes: 0,
        ..SessionRegistryConfig::default()
    };
    let error = SessionRegistry::new_production(config, production_supervisor())
        .expect_err("production registry must reject zero observation limits");
    assert_eq!(error.code, "observation_limits_invalid");

    for config in [
        SessionRegistryConfig {
            shell_command: hermetic_shell(),
            worker_runtime_root: Some(PathBuf::from("/run/user/1000/pohunek/workers")),
            worker_state_root: Some(PathBuf::from("/home/user/.local/state/pohunek/workers")),
            supervision: Some(production_supervision()),
            observation_output_wait: Duration::from_millis(u64::from(
                protocol::MAX_SESSION_WAIT_MS + 1,
            )),
            ..SessionRegistryConfig::default()
        },
        SessionRegistryConfig {
            shell_command: hermetic_shell(),
            worker_runtime_root: Some(PathBuf::from("/run/user/1000/pohunek/workers")),
            worker_state_root: Some(PathBuf::from("/home/user/.local/state/pohunek/workers")),
            supervision: Some(production_supervision()),
            session_wait: Duration::from_millis(u64::from(protocol::MAX_SESSION_WAIT_MS + 1)),
            ..SessionRegistryConfig::default()
        },
    ] {
        let error = SessionRegistry::new_production(config, production_supervisor())
            .expect_err("production registry must reject waits above the shared ceiling");
        assert_eq!(error.code, "observation_limits_invalid");
    }
}

#[test]
fn allocated_session_ids_are_prefixed_ulids() {
    let first = SessionRegistry::allocate_session_id();
    let second = SessionRegistry::allocate_session_id();

    assert_ne!(
        first, second,
        "separate allocations must not reuse a worker slot"
    );
    for id in [first, second] {
        let suffix = id.0.strip_prefix("s-").expect("session id prefix");
        ulid::Ulid::from_string(suffix).expect("valid session ULID");
    }
}

fn metadata(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn metadata_patch(entries: &[(&str, Option<&str>)]) -> BTreeMap<String, Option<String>> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.map(str::to_owned)))
        .collect()
}

/// A plain attach (no self-feed origin) for the given session id.
fn attach_params(id: &SessionId) -> SessionAttachParams {
    SessionAttachParams {
        session_id: id.clone(),
        initial_dimensions: None,
        origin_session_id: None,
        origin_daemon_id: None,
        origin_worker_id: None,
    }
}

#[test]
fn old_worker_attach_failure_has_an_actionable_recovery_hint() {
    let error = worker_error_to_protocol(WorkerError::AttachSnapshotUnsupported {
        selected_version: pohunek_worker_protocol::PREVIOUS_VERSION,
    });

    assert_eq!(error.code, "attach_snapshot_unsupported");
    assert!(error
        .recover
        .as_deref()
        .is_some_and(|hint| hint.contains("restart") && hint.contains("fork")));
}

#[tokio::test]
async fn managed_observation_returns_runtime_bound_screen_output_and_wait() {
    use base64::prelude::{Engine as _, BASE64_STANDARD};

    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "printf observation-ready; sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");

    let screen = registry.screen(&created.id).await.expect("screen snapshot");
    assert_eq!(screen.session_id, created.id);
    assert_eq!(
        screen.runtime.runtime_generation(),
        RuntimeGeneration::new(1)
    );

    let output = registry
        .output(
            &SessionOutputParams::new(
                created.id.clone(),
                Some(screen.runtime.clone()),
                Some(OutputOffset::new(0)),
                4096,
                Some(1_000),
            )
            .expect("output params"),
        )
        .await
        .expect("output page");
    let decoded = BASE64_STANDARD
        .decode(output.data_base64())
        .expect("valid output base64");
    assert!(decoded
        .windows(b"observation-ready".len())
        .any(|window| { window == b"observation-ready" }));
    assert_eq!(output.runtime(), &screen.runtime);

    let wait = registry
        .wait(
            &SessionWaitParams::new(
                created.id.clone(),
                None,
                None,
                None,
                None,
                Some(vec![SessionState::Running]),
                None,
                100,
            )
            .expect("wait params"),
        )
        .await
        .expect("already-satisfied wait");
    assert_eq!(wait.reason, SessionWaitReason::StateMatched);

    let stale_runtime = SessionRuntimeIdentity::new(
        screen.runtime.worker_instance_id(),
        RuntimeGeneration::new(screen.runtime.runtime_generation().get() + 1),
    )
    .expect("stale runtime identity");
    let stale = registry
        .output(
            &SessionOutputParams::new(
                created.id.clone(),
                Some(stale_runtime),
                Some(OutputOffset::new(0)),
                16,
                None,
            )
            .expect("stale output params"),
        )
        .await
        .expect_err("stale generation must fail");
    assert_eq!(stale.code, "session_runtime_changed");

    let _ = registry.stop(&created.id).await;
}

// Linux keeps the session's terminal usable for descendants after the
// session leader exits; XNU revokes it (`proc_exit`), so this drain
// behavior exists only on Linux. The worker's Darwin counterpart is
// `root_exit_revokes_the_terminal_and_stop_still_ends_the_group`.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn managed_output_remains_available_until_descendant_pty_eof() {
    use base64::prelude::{Engine as _, BASE64_STANDARD};

    let barrier = pohunek_test_support::tempdir().expect("create output barrier");
    let root_exited = barrier.path().join("root-exited");
    let release = barrier.path().join("release");
    let root_exited_arg = root_exited.to_string_lossy().into_owned();
    let release_arg = release.to_string_lossy().into_owned();
    let barrier_arg = barrier.path().to_string_lossy().into_owned();
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new(
            "/bin/sh",
            [
                "-c",
                concat!(
                    "trap '' HUP; root_pid=$$; (",
                    "while [ \"$(sed -n 's/^.*) \\([^ ]\\).*/\\1/p' \"/proc/$root_pid/stat\" 2>/dev/null)\" != Z ]; do sleep 0.01; done; ",
                    "printf exited > \"$1\"; ",
                    "while [ ! -e \"$2\" ] && [ -d \"$3\" ]; do sleep 0.01; done; ",
                    "if [ -e \"$2\" ]; then printf 'late-descendant-output\\n'; fi",
                    ") &"
                ),
                "pohunek-descendant-output",
                &root_exited_arg,
                &release_arg,
                &barrier_arg,
            ],
        ),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    let live_runtime = created.runtime.as_ref().expect("live runtime");
    let runtime = SessionRuntimeIdentity::new(
        live_runtime
            .worker_instance_id
            .clone()
            .expect("live runtime identifier"),
        live_runtime.runtime_generation,
    )
    .expect("valid runtime identity");

    wait_until("the root exit marker", || async {
        root_exited.exists().then_some(())
    })
    .await;
    assert_eq!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect draining session")
            .state,
        SessionState::Running,
        "root exit must not hide output while a descendant holds the PTY"
    );

    let output_params = SessionOutputParams::new(
        created.id.clone(),
        Some(runtime),
        Some(OutputOffset::new(0)),
        4_096,
        Some(1_000),
    )
    .expect("output params");
    let output = registry.output(&output_params);
    tokio::pin!(output);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut output)
            .await
            .is_err(),
        "authorized output long-poll must remain registered after root exit"
    );
    fs::write(&release, b"release").expect("release descendant");

    let output = guard("the descendant output page", &mut output)
        .await
        .expect("public output page");
    let decoded = BASE64_STANDARD
        .decode(output.data_base64())
        .expect("valid output base64");
    assert!(
        decoded
            .windows(b"late-descendant-output".len())
            .any(|window| window == b"late-descendant-output"),
        "daemon output API lost bytes produced after root exit"
    );
    assert_eq!(
        registry
            .wait_for_exit(&created.id, HANG_GUARD)
            .await
            .expect("terminal session")
            .state,
        SessionState::Done
    );
}

// Linux keeps the session's terminal usable for descendants after the
// session leader exits; XNU revokes it (`proc_exit`), so this drain
// behavior exists only on Linux. The worker's Darwin counterpart is
// `root_exit_revokes_the_terminal_and_stop_still_ends_the_group`.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn stop_after_root_exit_terminates_a_descendant_that_keeps_the_pty_open() {
    let barrier = pohunek_test_support::tempdir().expect("create stop barrier");
    let root_exited = barrier.path().join("root-exited");
    let descendant_ready = barrier.path().join("descendant-ready");
    let root_exited_arg = root_exited.to_string_lossy().into_owned();
    let descendant_ready_arg = descendant_ready.to_string_lossy().into_owned();
    let barrier_arg = barrier.path().to_string_lossy().into_owned();
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new(
            "/bin/sh",
            [
                "-c",
                concat!(
                    "trap '' HUP; root_pid=$$; (",
                    "trap '' HUP TERM; printf ready > \"$2\"; ",
                    "while [ \"$(sed -n 's/^.*) \\([^ ]\\).*/\\1/p' \"/proc/$root_pid/stat\" 2>/dev/null)\" != Z ]; do sleep 0.01; done; ",
                    "printf exited > \"$1\"; ",
                    "while [ -d \"$3\" ]; do sleep 0.01; done",
                    ") & while [ ! -e \"$2\" ]; do sleep 0.01; done"
                ),
                "pohunek-stop-after-root-exit",
                &root_exited_arg,
                &descendant_ready_arg,
                &barrier_arg,
            ],
        ),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    wait_until("the root exit marker", || async {
        root_exited.exists().then_some(())
    })
    .await;

    let stopped = guard(
        "the public stop of the draining session",
        Box::pin(registry.stop(&created.id)),
    )
    .await
    .expect("stop draining session");
    assert!(stopped.stopped);
    assert_eq!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect stopped session")
            .state,
        SessionState::Stopped
    );
}

#[tokio::test]
async fn session_wait_wakes_for_metadata_and_state_and_returns_timeout() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");

    let metadata_params = SessionWaitParams::new(
        created.id.clone(),
        None,
        Some(created.updated_at.clone()),
        None,
        None,
        None,
        None,
        1_000,
    )
    .expect("metadata wait params");
    let metadata_registry = registry.clone();
    let metadata_wait = tokio::spawn(async move { metadata_registry.wait(&metadata_params).await });
    tokio::task::yield_now().await;
    registry
        .set_metadata(
            &created.id,
            BTreeMap::from([("phase".to_owned(), Some("review".to_owned()))]),
        )
        .await
        .expect("update metadata");
    let metadata = metadata_wait
        .await
        .expect("metadata waiter task")
        .expect("metadata waiter result");
    assert_eq!(metadata.reason, SessionWaitReason::SessionUpdated);

    let timeout = registry
        .wait(
            &SessionWaitParams::new(
                created.id.clone(),
                None,
                None,
                None,
                None,
                Some(vec![SessionState::Failed]),
                None,
                10,
            )
            .expect("timeout params"),
        )
        .await
        .expect("timeout is a normal wait result");
    assert_eq!(timeout.reason, SessionWaitReason::Timeout);

    let state_params = SessionWaitParams::new(
        created.id.clone(),
        None,
        None,
        None,
        None,
        Some(vec![SessionState::Stopped]),
        None,
        1_000,
    )
    .expect("state wait params");
    let state_registry = registry.clone();
    let state_wait = tokio::spawn(async move { state_registry.wait(&state_params).await });
    let live_runtime = created
        .runtime
        .as_ref()
        .and_then(|runtime| {
            Some(SessionRuntimeIdentity::new(
                runtime.worker_instance_id.as_deref()?,
                runtime.runtime_generation,
            ))
        })
        .expect("live runtime identity")
        .expect("valid runtime identity");
    let runtime_params = SessionWaitParams::new(
        created.id.clone(),
        Some(live_runtime),
        None,
        None,
        None,
        None,
        None,
        1_000,
    )
    .expect("runtime wait params");
    let runtime_registry = registry.clone();
    let runtime_wait = tokio::spawn(async move { runtime_registry.wait(&runtime_params).await });
    tokio::task::yield_now().await;
    registry.stop(&created.id).await.expect("stop session");
    let state = state_wait
        .await
        .expect("state waiter task")
        .expect("state waiter result");
    assert_eq!(state.reason, SessionWaitReason::StateMatched);
    let runtime = runtime_wait
        .await
        .expect("runtime waiter task")
        .expect("runtime waiter result");
    assert_eq!(runtime.reason, SessionWaitReason::RuntimeChanged);
}

async fn assert_runtime_change_precedes_cursor_access(
    registry: &SessionRegistry,
    session_id: &SessionId,
    runtime: &SessionRuntimeIdentity,
) {
    for (watermark, output) in [
        (Some(TerminalWatermark::new(0)), None),
        (None, Some(OutputOffset::new(0))),
    ] {
        let result = registry
            .wait(
                &SessionWaitParams::new(
                    session_id.clone(),
                    Some(runtime.clone()),
                    None,
                    watermark,
                    output,
                    None,
                    None,
                    100,
                )
                .expect("composite runtime/cursor wait"),
            )
            .await
            .expect("runtime change wins before terminal access");
        assert_eq!(result.reason, SessionWaitReason::RuntimeChanged);
    }
}

#[tokio::test]
async fn composite_wait_short_circuits_ended_lost_and_disappeared_runtimes() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });

    let ended = registry
        .create(params())
        .await
        .expect("create ended session");
    let ended_runtime = SessionRuntimeIdentity::new(
        ended
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.worker_instance_id.as_deref())
            .expect("ended runtime id"),
        ended
            .runtime
            .as_ref()
            .expect("ended runtime")
            .runtime_generation,
    )
    .expect("ended runtime identity");
    registry.stop(&ended.id).await.expect("end session");
    assert_runtime_change_precedes_cursor_access(&registry, &ended.id, &ended_runtime).await;

    for runtime_case in [RuntimeState::Lost, RuntimeState::Reconnecting] {
        let created = registry
            .create(params())
            .await
            .expect("create live session");
        let expected = SessionRuntimeIdentity::new(
            created
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.worker_instance_id.as_deref())
                .expect("live runtime id"),
            created
                .runtime
                .as_ref()
                .expect("live runtime")
                .runtime_generation,
        )
        .expect("live runtime identity");
        let (runtime_handle, runtime_info) = {
            let mut sessions = registry.inner.sessions.lock().await;
            let entry = sessions.get_mut(&created.id).expect("registered session");
            let handle = entry.runtime.clone();
            let info = entry.info.runtime.clone();
            if runtime_case == RuntimeState::Lost {
                entry.runtime = RuntimeHandle::Unavailable(RuntimeState::Lost);
                entry.info.runtime.as_mut().expect("runtime").state = RuntimeState::Lost;
            } else {
                entry.info.runtime = None;
            }
            (handle, info)
        };
        assert_runtime_change_precedes_cursor_access(&registry, &created.id, &expected).await;
        let () = {
            let mut sessions = registry.inner.sessions.lock().await;
            let entry = sessions.get_mut(&created.id).expect("registered session");
            entry.runtime = runtime_handle;
            entry.info.runtime = runtime_info;
        };
        let _ = registry.stop(&created.id).await;
    }
}

#[cfg(unix)]
const LIVE_IDENTITY_REPORTER: &str = r#"import datetime
import json
import os
import socket
import sys
import time

reporter_pid = os.fork()
if reporter_pid != 0:
    time.sleep(30)
    raise SystemExit(0)

pid = os.getpid()
# The test releases each step after it observed the previous projection, so no
# projected state depends on how long the test takes to look at it.
gate = open(sys.argv[1], encoding="ascii")
failure_path = sys.argv[1] + ".failure"

def record_failure(kind, value, trace):
    # The test reads this file, so the reporter never dies without a reason.
    import traceback
    with open(failure_path, "w", encoding="utf-8") as handle:
        handle.write("".join(traceback.format_exception(kind, value, trace)))

sys.excepthook = record_failure

def wait_for_test():
    gate.readline()

def process_start_identity(pid):
    if sys.platform == "darwin":
        # `struct proc_bsdinfo` from `proc_pidinfo(PROC_PIDTBSDINFO)`: the
        # kernel start time the worker encodes as microseconds.
        import ctypes
        import struct

        libsystem = ctypes.CDLL("/usr/lib/libSystem.B.dylib")
        buffer = ctypes.create_string_buffer(136)
        assert libsystem.proc_pidinfo(pid, 3, ctypes.c_uint64(0), buffer, 136) == 136
        seconds, microseconds = struct.unpack_from("=QQ", buffer, 120)
        return seconds * 1_000_000 + microseconds
    with open(f"/proc/{pid}/stat", encoding="ascii") as handle:
        fields = handle.read().rsplit(")", 1)[1].split()
    return int(fields[19])

start_identity = process_start_identity(pid)
sequence = int(time.time() * 1000)

def send(request):
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.connect(os.environ["POHUNEK_WORKER_SOCKET_PATH"])
    client.sendall((json.dumps(request) + "\n").encode())
    received = b""
    while b"\n" not in received:
        chunk = client.recv(4096)
        if not chunk:
            raise RuntimeError(f"worker closed the socket before a full response line: {received!r}")
        received += chunk
    response = json.loads(received.split(b"\n", 1)[0])
    client.close()
    return response["ok"] is True

report = {
    "type": "identity_report",
    "runtime_id": os.environ["POHUNEK_WORKER_INSTANCE_ID"],
    "provider": "claude",
    "pid": pid,
    "start_identity": start_identity,
    "sequence": sequence,
    "expires_at": (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=30)).isoformat().replace("+00:00", "Z"),
    "reference_kind": "id",
    "native_reference": "live-native",
}
wrong_runtime = dict(report, runtime_id="wrong-runtime")
assert send(wrong_runtime) is False
unknown_provider = dict(report, provider="future-provider")
assert send(unknown_provider) is False
overlong = dict(report, expires_at=(datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=61)).isoformat().replace("+00:00", "Z"))
assert send(overlong) is False
wrong_process = dict(report, pid=1, start_identity=1)
assert send(wrong_process) is False
assert send(report) is True
duplicate = dict(report, native_reference="rejected-native")
assert send(duplicate) is False
stale = dict(report, sequence=sequence - 1, native_reference="rejected-stale")
assert send(stale) is False
wait_for_test()
assert send({
    "type": "identity_release",
    "runtime_id": os.environ["POHUNEK_WORKER_INSTANCE_ID"],
    "provider": "claude",
    "pid": pid,
    "start_identity": start_identity,
    "sequence": sequence + 1,
}) is True
late_after_release = dict(report, sequence=sequence + 1, native_reference="rejected-after-release")
assert send(late_after_release) is False
wait_for_test()
reasserted = dict(report, sequence=sequence + 2, native_reference="live-reasserted")
assert send(reasserted) is True
wait_for_test()
assert send({
    "type": "identity_release",
    "runtime_id": os.environ["POHUNEK_WORKER_INSTANCE_ID"],
    "provider": "claude",
    "pid": pid,
    "start_identity": start_identity,
    "sequence": sequence + 3,
}) is True
os._exit(0)
"#;

#[cfg(unix)]
#[tokio::test]
async fn worker_identity_changes_project_live_into_the_logical_session() {
    let root = temp_dir("live-worker-identity-projection");
    let reporter = root.join("identity_reporter.py");
    std::fs::write(&reporter, LIVE_IDENTITY_REPORTER).expect("write identity reporter");
    let gate_path = root.join("identity-steps.gate");
    let mut gate = hook_gate(&gate_path);
    let agents_dir = temp_agents_dir_with(
        "live-worker-identity-projection",
        "identity-live",
        &format!(
            "base = \"claude\"\nprogram = \"python3\"\nargs = [\"{}\", \"{}\"]\n",
            reporter.display(),
            gate_path.display()
        ),
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        agents_dir: Some(agents_dir),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(SessionNewParams {
            agent: "identity-live".to_owned(),
            cwd: Some(root),
            ..params()
        })
        .await
        .expect("create identity projection session");
    // The reporter is not a recognizable agent process, so the session's live
    // procwatch would clear the projected claim on its next tick and race the
    // waits below. This test covers the worker-identity projection only.
    registry
        .inner
        .sessions
        .lock()
        .await
        .get(&created.id)
        .expect("created session entry")
        .procwatch_cancel
        .cancel();

    // Each step of the reporter runs only after the test released it, so every
    // projected state stays in place until the test observed it.
    let assert_reporter_alive = || {
        if let Ok(failure) = fs::read_to_string(format!("{}.failure", gate_path.display())) {
            panic!("the identity reporter failed: {failure}");
        }
    };
    wait_until("the worker identity report to be projected", || async {
        assert_reporter_alive();
        let info = registry.inspect(&created.id).await.expect("inspect active");
        (info.active_agent.as_deref() == Some("claude")
            && info.active_agent_session_id.as_deref() == Some("live-native"))
        .then_some(())
    })
    .await;

    gate.write_all(b"release\n")
        .expect("release the identity release step");
    wait_until("the worker identity release to be projected", || async {
        assert_reporter_alive();
        let info = registry
            .inspect(&created.id)
            .await
            .expect("inspect release");
        (info.active_agent.is_none() && info.active_agent_session_id.is_none()).then_some(())
    })
    .await;

    gate.write_all(b"reassert\n")
        .expect("release the identity reassertion step");
    wait_until(
        "the higher-sequence worker identity to be reasserted",
        || async {
            assert_reporter_alive();
            let info = registry
                .inspect(&created.id)
                .await
                .expect("inspect reassertion");
            (info.active_agent_session_id.as_deref() == Some("live-reasserted")).then_some(())
        },
    )
    .await;

    gate.write_all(b"final-release\n")
        .expect("release the final identity release step");
    wait_until(
        "the final worker identity release to be projected",
        || async {
            assert_reporter_alive();
            let info = registry
                .inspect(&created.id)
                .await
                .expect("inspect final release");
            info.active_agent.is_none().then_some(())
        },
    )
    .await;

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn session_read_returns_visible_tail_and_truthful_source_fallbacks() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new(
            "/bin/sh",
            ["-c", "printf 'alpha\\nbeta\\ngamma'; sleep 30"],
        ),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");

    let read = |source: SessionReadSource, lines: Option<u32>| {
        let id = created.id.clone();
        SessionReadParams::new(id, Some(source), lines, None).expect("valid read params")
    };
    let visible = wait_until("the shell output", || async {
        let visible = registry
            .session_read(&read(SessionReadSource::Visible, Some(2)))
            .await
            .expect("visible read");
        (visible.text == "beta\ngamma").then_some(visible)
    })
    .await;
    assert_eq!(visible.text, "beta\ngamma");
    assert!(visible.truncated);
    assert_eq!(visible.source_used, SessionReadSource::Visible);
    assert!(!visible.alternate_screen);
    assert_eq!(
        visible.runtime.runtime_generation(),
        RuntimeGeneration::new(1)
    );

    let recent = registry
        .session_read(&read(SessionReadSource::Recent, None))
        .await
        .expect("recent read");
    assert_eq!(recent.source_used, SessionReadSource::Visible);
    assert_eq!(recent.text, "alpha\nbeta\ngamma");
    assert!(!recent.alternate_screen);

    let unwrapped = registry
        .session_read(&read(SessionReadSource::RecentUnwrapped, None))
        .await
        .expect("unwrapped read");
    assert_eq!(unwrapped.source_used, SessionReadSource::Visible);
    assert_eq!(unwrapped.text, recent.text);
    assert!(!unwrapped.alternate_screen);

    let detection = registry
        .session_read(&read(SessionReadSource::Detection, Some(1)))
        .await
        .expect("detection read");
    assert_eq!(detection.source_used, SessionReadSource::Visible);
    assert_eq!(detection.text, "gamma");
    assert!(detection.truncated);

    let ansi = SessionReadParams::new(
        created.id.clone(),
        None,
        None,
        Some(SessionReadFormat::Ansi),
    )
    .expect("valid ANSI params");
    let ansi_error = registry
        .session_read(&ansi)
        .await
        .expect_err("ANSI is unavailable");
    assert_eq!(ansi_error.code, "session_read_ansi_unavailable");

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn session_read_preserves_alternate_screen_state_during_fallback() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new(
            "/bin/sh",
            ["-c", "printf '\\033[?1049hTUI-one\\nTUI-two'; sleep 30"],
        ),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    let params = SessionReadParams::new(
        created.id.clone(),
        Some(SessionReadSource::RecentUnwrapped),
        None,
        None,
    )
    .expect("read params");

    let result = wait_until("the alternate-screen output", || async {
        let result = registry
            .session_read(&params)
            .await
            .expect("read alternate screen");
        (result.alternate_screen && result.text == "TUI-one\nTUI-two").then_some(result)
    })
    .await;

    assert_eq!(result.source_used, SessionReadSource::Visible);
    assert!(result.alternate_screen);
    assert_eq!(result.text, "TUI-one\nTUI-two");

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn session_read_identity_guard_rejects_runtime_replacement_race() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "printf ready; sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    let observed = registry
        .managed_session(&created.id)
        .await
        .expect("managed session");
    let replacement_generation = RuntimeGeneration::new(observed.runtime_generation.get() + 1);
    {
        let mut sessions = registry.inner.sessions.lock().await;
        sessions
            .get_mut(&created.id)
            .and_then(|entry| entry.info.runtime.as_mut())
            .expect("live runtime")
            .runtime_generation = replacement_generation;
    };

    let error = registry
        .verify_managed_identity(&created.id, &observed)
        .await
        .expect_err("replaced runtime must invalidate an in-flight snapshot");

    assert_eq!(error.code, "session_runtime_changed");
    {
        let mut sessions = registry.inner.sessions.lock().await;
        let entry = sessions.get_mut(&created.id).expect("registered session");
        entry
            .info
            .runtime
            .as_mut()
            .expect("live runtime")
            .runtime_generation = observed.runtime_generation;
        entry.info.state = SessionState::Done;
        entry.info.runtime.as_mut().expect("terminal runtime").state = RuntimeState::Terminal;
    };
    registry
        .verify_managed_identity(&created.id, &observed)
        .await
        .expect("same in-flight runtime remains valid after terminal transition");
    let mut sessions = registry.inner.sessions.lock().await;
    let entry = sessions.get_mut(&created.id).expect("registered session");
    entry.info.state = SessionState::Running;
    entry.info.runtime.as_mut().expect("live runtime").state = RuntimeState::Live;
    drop(sessions);
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn mutations_fail_closed_for_a_session_whose_agent_kind_is_not_launchable() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    // A valid runtime id no definition backs is `runtime_not_installed`; a
    // value outside the runtime-id grammar stays `agent_kind_unsupported`.
    for (kind, code) in [
        ("future-agent", "runtime_not_installed"),
        ("Future Agent", "agent_kind_unsupported"),
    ] {
        registry
            .inner
            .sessions
            .lock()
            .await
            .get_mut(&created.id)
            .expect("registered session")
            .info
            .agent_base = RuntimeRef::from_wire(kind);

        let error = registry
            .stop(&created.id)
            .await
            .expect_err("unlaunchable agent mutation must fail closed");
        assert_eq!(error.code, code, "{kind}");
        assert_eq!(
            registry
                .inspect(&created.id)
                .await
                .expect("inspect unchanged session")
                .state,
            SessionState::Running
        );
    }

    registry
        .inner
        .sessions
        .lock()
        .await
        .get_mut(&created.id)
        .expect("registered session")
        .info
        .agent_base = RuntimeRef::shell();
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn a_launched_session_freezes_the_launch_binding_of_its_runtime() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");

    let expected = registry
        .inner
        .profiles
        .runtimes()
        .resolve_ref(&RuntimeRef::shell())
        .expect("shell resolves")
        .binding()
        .clone();
    let pin = {
        let sessions = registry.inner.sessions.lock().await;
        let entry = sessions.get(&created.id).expect("registered session");
        SessionRegistry::resume_binding_from_entry(&created.id, entry).launch_binding
    };
    assert_eq!(
        pin,
        crate::agent::host::LaunchPin::Pinned(Box::new(expected))
    );
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn resume_refuses_a_runtime_it_cannot_resolve_or_whose_pin_does_not_match() {
    use crate::agent::host::LaunchPin;
    use protocol::{
        BindingProvenance, LaunchBinding, PackageDigest, PackageId, PackageIdentity,
        PackageVersion, RuntimeId,
    };

    let registry = SessionRegistry::default();
    let binding = |agent_base: RuntimeRef, launch_binding: LaunchPin| crate::store::ResumeBinding {
        session_id: "s-pin".to_owned(),
        name: None,
        agent: agent_base.as_wire().to_owned(),
        agent_base,
        cwd: temp_dir("pin-binding-cwd"),
        cols: 80,
        rows: 24,
        native_session_id: Some("native-ignored".to_owned()),
        native_session_path: None,
        project_id: None,
        is_linked_worktree: None,
        metadata: BTreeMap::new(),
        program: "/bin/sh".to_owned(),
        args: Vec::new(),
        input_rules: crate::store::StoredInputRules::default(),
        native_launch: None,
        launch_binding,
        native_reference_provenance: crate::agent::NativeReferenceProvenance::default(),
        profile_revision: None,
        native_launch_unresolved: false,
    };
    let package_pin = LaunchPin::Pinned(Box::new(LaunchBinding {
        runtime_id: RuntimeId::parse("claude").expect("runtime id"),
        provenance: BindingProvenance::Package {
            package: PackageIdentity {
                id: PackageId::parse("acme.runtime").expect("package id"),
                version: PackageVersion::parse("1.0.0").expect("version"),
            },
            package_digest: PackageDigest::parse(
                "sha256:4444444444444444444444444444444444444444444444444444444444444444",
            )
            .expect("digest"),
        },
    }));
    let cases = [
        // Not installed, with and without a recorded pin.
        (
            RuntimeRef::from_wire("acme"),
            LaunchPin::Unpinned,
            "runtime_not_installed",
        ),
        (
            RuntimeRef::from_wire("acme"),
            package_pin.clone(),
            "runtime_not_installed",
        ),
        // Outside the runtime-id grammar.
        (
            RuntimeRef::from_wire("Acme Agent"),
            LaunchPin::Unpinned,
            "agent_kind_unsupported",
        ),
        // Resolves, but a package pin cannot be served by the built-in.
        (RuntimeRef::claude(), package_pin, "runtime_not_installed"),
        // Resolves and the pin matches (unpinned lines resume through a
        // built-in); the binding is then refused only for lacking a native
        // launch spec, which is checked after the runtime.
        (
            RuntimeRef::claude(),
            LaunchPin::Unpinned,
            "agent_not_resumable",
        ),
    ];
    for (kind, pin, code) in cases {
        let label = format!("{kind:?}");
        let err = registry
            .resume_binding(binding(kind, pin))
            .await
            .expect_err("binding must be refused");
        assert_eq!(err.code, code, "{label}");
    }
}

#[tokio::test]
async fn the_shell_command_text_never_breaks_the_registry_or_the_snapshot_program() {
    // A shell command is host input: control characters and over-long text are
    // launch argv, not descriptor data, so the registry still builds and the
    // shell runtime's snapshot program stays the host login shell.
    let long_script = "x".repeat(crate::agent::host::MAX_ARG_BYTES * 2);
    let command = ShellCommand::new(
        "/bin/sh",
        vec![
            "-c".to_owned(),
            format!("printf 'a\nb'; sleep 30 # {long_script}"),
        ],
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: command,
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    let (snapshot_program, snapshot_args) = {
        let sessions = registry.inner.sessions.lock().await;
        let entry = sessions.get(&created.id).expect("registered session");
        (entry.snapshot.program.clone(), entry.snapshot.args.clone())
    };
    assert_eq!(snapshot_program, crate::agent::host_login_shell());
    assert!(snapshot_args.is_empty());

    // A shell-base profile without `program` launches the login shell, not
    // the configured shell command.
    let resolved = registry
        .inner
        .profiles
        .resolve_agent("shell")
        .expect("shell resolves");
    assert_eq!(resolved.program(), crate::agent::host_login_shell());
    let _ = registry.stop(&created.id).await;
}

/// Returns the metadata store path inside a fresh private directory that is
/// removed when the calling test thread ends.
pub(super) fn temp_store_path(tag: &str) -> PathBuf {
    crate::test_support::thread_scoped_dir(&format!("pohunek-session-{tag}-"))
        .join("metadata.jsonl")
}

std::thread_local! {
    /// Worker roots of the fixtures built on the current test thread, removed
    /// when it ends.
    static WORKER_ROOT_DIRS: std::cell::RefCell<Vec<tempfile::TempDir>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Resolves the worker roots of a fixture that builds its own launcher.
///
/// Roots the config does not name are fresh directories that live until the
/// calling test thread ends, because the fixture outlives any one registry.
fn test_worker_roots(config: &SessionRegistryConfig) -> (PathBuf, PathBuf) {
    let (runtime_root, state_root, owned) = super::owned_test_worker_roots(config);
    WORKER_ROOT_DIRS.with(|dirs| dirs.borrow_mut().extend(owned));
    (runtime_root, state_root)
}

fn test_native_launch(kind: SessionRefKind, fork: bool) -> NativeSessionLaunch {
    NativeSessionLaunch::from_templates(
        kind,
        &["--resume", "{reference}"],
        fork.then_some(&["--resume", "{reference}", "--fork-session"][..]),
    )
    .expect("valid test templates")
}

pub(super) fn temp_dir(tag: &str) -> PathBuf {
    let dir = temp_store_path(tag)
        .parent()
        .expect("store parent")
        .join("dir");
    fs::create_dir_all(&dir).expect("create temp dir");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("secure temp directory");
    dir
}

fn write_host_hook(config_dir: &std::path::Path, event: &str, body: &str) {
    let hooks = config_dir.join("hooks");
    fs::create_dir_all(&hooks).expect("create hooks dir");
    fs::write(hooks.join(event), body).expect("write hook");
}

#[cfg(unix)]
pub(super) fn write_executable(path: &std::path::Path, body: &str) {
    fs::write(path, body).expect("write executable");
    let mut perms = fs::metadata(path).expect("metadata").permissions();
    perms.set_mode(0o700);
    fs::set_permissions(path, perms).expect("chmod executable");
}

#[cfg(unix)]
fn write_supported_hermes_executable(path: &std::path::Path, launch_body: &str) {
    write_executable(
        path,
        &format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  printf '%s\\n' 'Hermes Agent v0.20.0'\n  exit 0\nfi\n{launch_body}"
        ),
    );
}

#[cfg(unix)]
fn write_resume_agent_script(path: &std::path::Path, marker: &std::path::Path) {
    write_executable(
        path,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> {}\nsleep 30\n",
            marker.display()
        ),
    );
}

#[cfg(unix)]
fn terminate_pid(pid: u32) {
    let _ = pohunek_test_support::process_env::command("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status();
}

pub(super) async fn wait_for_file_contains(path: &std::path::Path, needle: &str) -> String {
    wait_until(
        &format!("{} to contain {needle:?}", path.display()),
        || async {
            fs::read_to_string(path)
                .ok()
                .filter(|contents| contents.contains(needle))
        },
    )
    .await
}

async fn wait_for_line_count(path: &std::path::Path, expected: usize) -> String {
    wait_until(
        &format!("{} to contain at least {expected} lines", path.display()),
        || async {
            fs::read_to_string(path)
                .ok()
                .filter(|contents| contents.lines().count() >= expected)
        },
    )
    .await
}

async fn wait_for_resume_binding_removed(registry: &SessionRegistry, id: &SessionId) {
    wait_until(&format!("resume binding removal for {}", id.0), || async {
        let bindings = registry
            .inner
            .store
            .as_ref()
            .expect("registry store")
            .load_resume()
            .expect("load resume bindings");
        bindings
            .iter()
            .all(|binding| binding.session_id != id.0)
            .then_some(())
    })
    .await;
}

fn transition(activity: AgentActivity) -> ActivityTransition {
    ActivityTransition {
        activity,
        source: protocol::StateSource::Process,
    }
}

#[test]
fn external_observer_defaults_off() {
    assert!(
        !SessionRegistryConfig::default().observe_external_agents,
        "external observation watches provider transcript trees and must remain opt-in"
    );
}

#[derive(Debug, Default)]
struct MockInspector {
    inner: Mutex<MockInspectorState>,
}

#[derive(Debug, Default)]
struct MockInspectorState {
    descendants: HashMap<Pid, Vec<ProcessFact>>,
    descendants_error: Option<std::io::ErrorKind>,
    foreground_groups: HashMap<Pid, Option<Pid>>,
    foreground_error: Option<std::io::ErrorKind>,
    foreground_block: Option<Arc<ForegroundBlock>>,
    cwd: HashMap<Pid, PathBuf>,
    identity_overrides: HashMap<Pid, Option<ProcessIdentity>>,
    identity_after_descendants: HashMap<Pid, ProcessIdentity>,
    identity_after_cwd: HashMap<Pid, (usize, ProcessIdentity)>,
    identity_errors: HashMap<Pid, std::io::ErrorKind>,
    non_running: HashSet<ProcessIdentity>,
    exits: HashMap<Pid, Vec<tokio::sync::watch::Sender<bool>>>,
    immediate_exits: HashSet<Pid>,
    events_expected_before_watch: HashMap<Pid, tokio::sync::broadcast::Receiver<Event>>,
    ownership_markers: HashMap<Pid, OwnershipMarkers>,
}

#[derive(Debug, Default)]
struct ForegroundBlock {
    entered: AtomicBool,
    released: AtomicBool,
}

impl MockInspector {
    /// Returns the scripted fact for `pid`, if any test registered one.
    fn fact(&self, pid: Pid) -> Option<ProcessFact> {
        self.inner
            .lock()
            .expect("mock inspector lock")
            .descendants
            .values()
            .flatten()
            .find(|fact| fact.pid == pid)
            .cloned()
    }

    fn set_descendants(&self, root: Pid, facts: Vec<ProcessFact>) {
        let mut inner = self.inner.lock().expect("mock inspector lock");
        for fact in &facts {
            inner
                .cwd
                .entry(fact.pid)
                .or_insert_with(|| PathBuf::from("/work"));
        }
        inner.descendants.insert(root, facts);
    }

    fn fail_descendants_with(&self, kind: std::io::ErrorKind) {
        self.inner
            .lock()
            .expect("mock inspector lock")
            .descendants_error = Some(kind);
    }

    fn clear_descendants_error(&self) {
        self.inner
            .lock()
            .expect("mock inspector lock")
            .descendants_error = None;
    }

    fn fail_foreground_with(&self, kind: std::io::ErrorKind) {
        self.inner
            .lock()
            .expect("mock inspector lock")
            .foreground_error = Some(kind);
    }

    fn clear_foreground_error(&self) {
        self.inner
            .lock()
            .expect("mock inspector lock")
            .foreground_error = None;
    }

    fn block_foreground(&self) -> Arc<ForegroundBlock> {
        let block = Arc::new(ForegroundBlock::default());
        self.inner
            .lock()
            .expect("mock inspector lock")
            .foreground_block = Some(Arc::clone(&block));
        block
    }

    fn fire_exit(&self, pid: Pid) {
        let mut inner = self.inner.lock().expect("mock inspector lock");
        for sender in inner.exits.entry(pid).or_default() {
            sender.send_replace(true);
        }
    }

    fn set_immediate_exit(&self, pid: Pid) {
        self.inner
            .lock()
            .expect("mock inspector lock")
            .immediate_exits
            .insert(pid);
    }

    fn expect_created_before_exit_watch(
        &self,
        pid: Pid,
        events: tokio::sync::broadcast::Receiver<Event>,
    ) {
        self.inner
            .lock()
            .expect("mock inspector lock")
            .events_expected_before_watch
            .insert(pid, events);
    }

    fn fire_exit_watch(&self, pid: Pid, generation: usize) {
        let sender = self
            .inner
            .lock()
            .expect("mock inspector lock")
            .exits
            .get(&pid)
            .and_then(|watches| watches.get(generation))
            .cloned()
            .expect("exit watch generation");
        sender.send_replace(true);
    }

    fn exit_watch_count(&self, pid: Pid) -> usize {
        self.inner
            .lock()
            .expect("mock inspector lock")
            .exits
            .get(&pid)
            .map_or(0, Vec::len)
    }

    fn set_cwd(&self, pid: Pid, cwd: PathBuf) {
        self.inner
            .lock()
            .expect("mock inspector lock")
            .cwd
            .insert(pid, cwd);
    }

    fn set_identity(&self, pid: Pid, identity: Option<ProcessIdentity>) {
        self.inner
            .lock()
            .expect("mock inspector lock")
            .identity_overrides
            .insert(pid, identity);
    }

    fn set_not_running(&self, identity: ProcessIdentity) {
        self.inner
            .lock()
            .expect("mock inspector lock")
            .non_running
            .insert(identity);
    }

    fn change_identity_after_descendants(&self, pid: Pid, identity: ProcessIdentity) {
        self.inner
            .lock()
            .expect("mock inspector lock")
            .identity_after_descendants
            .insert(pid, identity);
    }

    fn change_identity_after_cwd(&self, pid: Pid, identity: ProcessIdentity) {
        self.inner
            .lock()
            .expect("mock inspector lock")
            .identity_after_cwd
            .insert(pid, (2, identity));
    }

    fn fail_identity_with(&self, pid: Pid, kind: std::io::ErrorKind) {
        self.inner
            .lock()
            .expect("mock inspector lock")
            .identity_errors
            .insert(pid, kind);
    }

    fn set_ownership_markers(&self, pid: Pid, markers: OwnershipMarkers) {
        self.inner
            .lock()
            .expect("mock inspector lock")
            .ownership_markers
            .insert(pid, markers);
    }

    fn set_foreground_group(&self, pid: Pid, group: Option<Pid>) {
        self.inner
            .lock()
            .expect("mock inspector lock")
            .foreground_groups
            .insert(pid, group);
    }
}

impl ProcessInspector for MockInspector {
    fn identity(&self, pid: Pid) -> Result<Option<ProcessIdentity>, crate::procwatch::Error> {
        if let Some(kind) = self
            .inner
            .lock()
            .expect("mock inspector lock")
            .identity_errors
            .get(&pid)
            .copied()
        {
            return Err(crate::procwatch::Error::from_io(
                "mock_identity",
                std::io::Error::new(kind, "mock identity failure"),
            ));
        }
        if let Some(identity) = self
            .inner
            .lock()
            .expect("mock inspector lock")
            .identity_overrides
            .get(&pid)
            .copied()
        {
            return Ok(identity);
        }
        match self.fact(pid) {
            Some(fact) => Ok(Some(fact.identity())),
            // A real process (the session root) is identified the way the
            // production liveness checks do it: from the kernel process record
            // alone. A full `process` read also parses the argument region,
            // which Darwin can serve malformed while the root is still inside
            // its `sh` -> `bash` -> command exec chain right after launch.
            None => crate::procwatch::HostInspector::new().identity(pid),
        }
    }

    fn is_running(&self, identity: ProcessIdentity) -> Result<bool, crate::procwatch::Error> {
        if self
            .inner
            .lock()
            .expect("mock inspector lock")
            .non_running
            .contains(&identity)
        {
            return Ok(false);
        }
        Ok(self.identity(identity.pid)? == Some(identity))
    }

    fn parent_pid(&self, pid: Pid) -> Result<Option<Pid>, crate::procwatch::Error> {
        self.process(pid).map(|fact| fact.map(|fact| fact.ppid))
    }

    fn process(&self, pid: Pid) -> Result<Option<ProcessFact>, crate::procwatch::Error> {
        match self.fact(pid) {
            Some(fact) => Ok(Some(fact)),
            None => crate::procwatch::HostInspector::new().process(pid),
        }
    }

    fn same_user_processes(&self) -> Result<Vec<ProcessFact>, crate::procwatch::Error> {
        let mut facts = self
            .inner
            .lock()
            .expect("mock inspector lock")
            .descendants
            .values()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        facts.sort_by_key(|fact| fact.pid);
        facts.dedup_by_key(|fact| fact.pid);
        Ok(facts)
    }

    fn descendants(&self, root: Pid) -> Result<Vec<ProcessFact>, crate::procwatch::Error> {
        let mut inner = self.inner.lock().expect("mock inspector lock");
        if let Some(kind) = inner.descendants_error {
            return Err(crate::procwatch::Error::from_io(
                "mock_descendants",
                std::io::Error::new(kind, "mock descendants failure"),
            ));
        }
        let descendants = inner.descendants.get(&root).cloned().unwrap_or_default();
        if let Some(identity) = inner.identity_after_descendants.remove(&root) {
            inner.identity_overrides.insert(root, Some(identity));
        }
        Ok(descendants)
    }

    fn cwd(&self, pid: Pid) -> Result<PathBuf, crate::procwatch::Error> {
        let mut inner = self.inner.lock().expect("mock inspector lock");
        let cwd = inner.cwd.get(&pid).cloned();
        if let Some((remaining, identity)) = inner.identity_after_cwd.get_mut(&pid) {
            if *remaining == 1 {
                let identity = *identity;
                inner.identity_after_cwd.remove(&pid);
                inner.identity_overrides.insert(pid, Some(identity));
            } else {
                *remaining -= 1;
            }
        }
        cwd.ok_or(crate::procwatch::Error::Race {
            operation: "mock_cwd",
        })
    }

    fn executable(&self, pid: Pid) -> Result<Option<PathBuf>, crate::procwatch::Error> {
        crate::procwatch::HostInspector::new().executable(pid)
    }

    fn exit_watch(&self, identity: ProcessIdentity) -> Result<ExitWatch, crate::procwatch::Error> {
        let (sender, receiver) = tokio::sync::watch::channel(false);
        let immediate = {
            let mut inner = self.inner.lock().expect("mock inspector lock");
            inner.exits.entry(identity.pid).or_default().push(sender);
            if let Some(mut events) = inner.events_expected_before_watch.remove(&identity.pid) {
                let event = events
                    .try_recv()
                    .expect("external creation must be published before its exit watch is armed");
                assert_eq!(event.event(), protocol::event::SESSION_CREATED);
            }
            inner.immediate_exits.contains(&identity.pid)
        };
        Ok(ExitWatch::from_future(async move {
            if immediate {
                return Ok(());
            }
            let mut receiver = receiver;
            while !*receiver.borrow_and_update() {
                receiver.changed().await.map_err(|_closed| {
                    crate::procwatch::Error::from_io(
                        "mock_exit_watch",
                        std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "test exit signal dropped",
                        ),
                    )
                })?;
            }
            Ok(())
        }))
    }

    fn ownership_markers(&self, pid: Pid) -> Result<OwnershipMarkers, crate::procwatch::Error> {
        Ok(self
            .inner
            .lock()
            .expect("mock inspector lock")
            .ownership_markers
            .get(&pid)
            .cloned()
            .unwrap_or_default())
    }

    fn foreground_process_group(
        &self,
        root_pid: Pid,
    ) -> Result<Option<Pid>, crate::procwatch::Error> {
        let (kind, foreground, block) = {
            let inner = self.inner.lock().expect("mock inspector lock");
            (
                inner.foreground_error,
                inner.foreground_groups.get(&root_pid).copied().flatten(),
                inner.foreground_block.clone(),
            )
        };
        if let Some(block) = block {
            block.entered.store(true, Ordering::Release);
            while !block.released.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
        }
        if let Some(kind) = kind {
            return Err(crate::procwatch::Error::from_io(
                "mock_foreground_process_group",
                std::io::Error::new(kind, "mock foreground failure"),
            ));
        }
        Ok(foreground)
    }
}

#[tokio::test]
async fn foreground_leader_selects_agent_and_shell_return_clears_claim() {
    let (registry, inspector, created) = mock_procwatch_registry("foreground-leader").await;
    let first = codex_fact(300, created.pid);
    let replacement = claude_fact(310, created.pid);
    inspector.set_descendants(created.pid, vec![first]);
    inspector.set_cwd(300, temp_dir("foreground-first"));
    inspector.set_foreground_group(created.pid, Some(300));
    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;

    let active = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(active.active_agent.as_deref(), Some("codex"));
    assert_eq!(active.active_agent_pid, Some(300));

    inspector.set_descendants(created.pid, vec![replacement]);
    inspector.set_cwd(310, temp_dir("foreground-replacement"));
    inspector.set_foreground_group(created.pid, Some(310));
    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;
    let rebound = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(rebound.active_agent.as_deref(), Some("claude"));
    assert_eq!(rebound.active_agent_pid, Some(310));

    inspector.set_foreground_group(created.pid, Some(created.pid));
    for _ in 0..DIRECT_AGENT_RESCAN_COUNT {
        registry
            .rescan_procwatch_at(&created.id, created.pid, Instant::now())
            .await;
        let cleared = registry.inspect(&created.id).await.expect("inspect");
        assert_eq!(cleared.active_agent, None);
        assert_eq!(cleared.active_agent_base, None);
        assert_eq!(cleared.active_agent_pid, None);
    }

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn shell_foreground_preserves_recent_unbound_claim_until_ttl() {
    let (registry, inspector, created) = mock_procwatch_registry("foreground-unbound-ttl").await;
    inspector.set_foreground_group(created.pid, Some(created.pid));
    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: CODEX_HOOK_SOURCE.to_owned(),
            agent: CODEX_HOOK_AGENT.to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(ReportSequence::new(DIRECT_AGENT_REPORT_SEQ)),
            pid: None,
            agent_session_id: None,
            agent_session_path: None,
        })
        .await;
    assert!(report.recorded);
    let reported_at = registry.inner.sessions.lock().await[&created.id]
        .active_agent
        .as_ref()
        .expect("active report")
        .reported_at;

    registry
        .rescan_procwatch_at(
            &created.id,
            created.pid,
            reported_at + Duration::from_millis(49),
        )
        .await;
    let retained = registry
        .inspect(&created.id)
        .await
        .expect("inspect retained");
    assert_eq!(retained.active_agent.as_deref(), Some(CODEX_HOOK_AGENT));

    registry
        .rescan_procwatch_at(
            &created.id,
            created.pid,
            reported_at + Duration::from_millis(50),
        )
        .await;
    let expired = registry
        .inspect(&created.id)
        .await
        .expect("inspect expired");
    assert_eq!(expired.active_agent, None);

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn root_foreground_group_selects_member_until_member_exits() {
    let (registry, inspector, created) = mock_procwatch_registry("foreground-root-member").await;
    let mut member = codex_fact(320, created.pid);
    member.pgid = created.pid;
    inspector.set_descendants(created.pid, vec![member]);
    inspector.set_foreground_group(created.pid, Some(created.pid));

    for _ in 0..DIRECT_AGENT_RESCAN_COUNT {
        registry
            .rescan_procwatch_at(&created.id, created.pid, Instant::now())
            .await;
        let selected = registry.inspect(&created.id).await.expect("inspect");
        assert_eq!(selected.active_agent.as_deref(), Some("codex"));
        assert_eq!(selected.active_agent_pid, Some(320));
    }

    inspector.set_descendants(created.pid, Vec::new());
    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;
    let cleared = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(cleared.active_agent, None);
    assert_eq!(cleared.active_agent_pid, None);

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn foreign_foreground_process_does_not_hijack_reconciliation() {
    let (registry, inspector, created) = mock_procwatch_registry("foreground-foreign").await;
    let mut foreign = codex_fact(FOREIGN_AGENT_PID, created.pid);
    foreign.pgid = FOREIGN_AGENT_PID;
    inspector.set_descendants(
        created.pid,
        vec![foreign, codex_fact(FOREIGN_AGENT_PID + 1, created.pid)],
    );
    inspector.set_ownership_markers(
        FOREIGN_AGENT_PID,
        OwnershipMarkers {
            daemon_id: Some("foreign-daemon".to_owned()),
            session_id: Some("foreign-session".to_owned()),
            worker_instance_id: None,
            runtime_id: None,
        },
    );
    let root_cwd = temp_dir("foreign-foreground-root");
    inspector.set_cwd(created.pid, root_cwd.clone());
    inspector.set_foreground_group(created.pid, Some(FOREIGN_AGENT_PID));

    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;
    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(inspected.active_agent, None);
    assert_eq!(inspected.active_agent_pid, None);
    assert_eq!(inspected.cwd, root_cwd);
}

#[tokio::test]
async fn foreground_member_matches_pgid_without_crossing_groups() {
    let (registry, inspector, created) = mock_procwatch_registry("foreground-member").await;

    // The unrecognized wrapper owns the foreground group. Only a recognized
    // member of that exact PGID is eligible; another group must never win.
    let mut wrapper = ProcessFact {
        comm: "wrapper".to_owned(),
        cmdline: vec!["/usr/bin/wrapper".to_owned()],
        ..codex_fact(400, created.pid)
    };
    wrapper.pgid = 399;
    let mut member = claude_fact(410, 400);
    member.pgid = 399;
    let unrelated = codex_fact(420, created.pid);
    inspector.set_descendants(created.pid, vec![wrapper, member, unrelated]);
    inspector.set_foreground_group(created.pid, Some(399));

    for _ in 0..DIRECT_AGENT_RESCAN_COUNT {
        registry
            .rescan_procwatch_at(&created.id, created.pid, Instant::now())
            .await;
        let selected = registry.inspect(&created.id).await.expect("inspect");

        assert_eq!(selected.active_agent.as_deref(), Some("claude"));
        assert_eq!(selected.active_agent_base, Some(RuntimeRef::claude()));
        assert_eq!(selected.active_agent_pid, Some(410));
    }

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn foreground_recognized_leader_wins_over_older_same_kind_other_group() {
    let (registry, inspector, created) = mock_procwatch_registry("foreground-exact-leader").await;
    inspector.set_descendants(created.pid, vec![codex_fact(430, created.pid)]);
    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;

    let mut leader = codex_fact(440, created.pid);
    leader.pgid = 440;
    let mut member = claude_fact(441, 440);
    member.pgid = 440;
    inspector.set_descendants(
        created.pid,
        vec![codex_fact(430, created.pid), member, leader],
    );
    inspector.set_foreground_group(created.pid, Some(440));

    for _ in 0..DIRECT_AGENT_RESCAN_COUNT {
        registry
            .rescan_procwatch_at(&created.id, created.pid, Instant::now())
            .await;
        let selected = registry.inspect(&created.id).await.expect("inspect");
        assert_eq!(selected.active_agent.as_deref(), Some("codex"));
        assert_eq!(selected.active_agent_pid, Some(440));
    }

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn foreground_replacement_replaces_hook_identity_and_detector() {
    let (registry, inspector, created) = mock_direct_codex_registry("foreground-replace").await;
    inspector.set_descendants(created.pid, vec![claude_fact(500, created.pid)]);
    inspector.set_cwd(500, temp_dir("foreground-replacement"));
    inspector.set_foreground_group(created.pid, Some(500));

    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: CODEX_HOOK_SOURCE.to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(ReportSequence::new(DIRECT_AGENT_REPORT_SEQ)),
            pid: Some(created.pid),
            agent_session_id: Some("stale-codex-native".to_owned()),
            agent_session_path: Some("/work/stale-codex.jsonl".to_owned()),
        })
        .await;
    assert!(report.recorded);
    let initial_detector = registry.inner.sessions.lock().await[&created.id]
        .detector_config
        .subscribe();
    assert!(initial_detector.borrow().config.manifest.is_some());

    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;
    let replaced = registry.inspect(&created.id).await.expect("inspect");
    let replacement_detector = registry.inner.sessions.lock().await[&created.id]
        .detector_config
        .subscribe();

    assert_eq!(replaced.active_agent.as_deref(), Some("claude"));
    assert_eq!(replaced.active_agent_base, Some(RuntimeRef::claude()));
    assert_eq!(replaced.active_agent_pid, Some(500));
    assert_eq!(replaced.active_agent_session_id, None);
    assert_eq!(replaced.active_agent_session_path, None);
    assert_eq!(replaced.activity, None);
    assert_eq!(replaced.state_source, StateSource::Process);
    assert_eq!(
        replacement_detector.borrow().config.detection,
        DetectorConfig::for_agent(&RuntimeHost::default(), &RuntimeRef::claude()).detection
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn first_foreground_scan_binds_hook_identity_without_replacing_metadata() {
    let (registry, inspector, created) = mock_procwatch_registry("foreground-hook-bind").await;
    let observed = codex_fact(510, created.pid);
    inspector.set_descendants(created.pid, vec![observed.clone()]);
    inspector.set_cwd(observed.pid, temp_dir("foreground-hook-bind"));
    inspector.set_foreground_group(created.pid, Some(observed.pid));

    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: CODEX_HOOK_SOURCE.to_owned(),
            agent: CODEX_HOOK_AGENT.to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(ReportSequence::new(DIRECT_AGENT_REPORT_SEQ)),
            pid: Some(observed.pid),
            agent_session_id: Some("hook-native-id".to_owned()),
            agent_session_path: Some("/work/hook-native.jsonl".to_owned()),
        })
        .await;
    assert!(report.recorded);
    assert_eq!(
        registry.inner.sessions.lock().await[&created.id]
            .active_agent
            .as_ref()
            .and_then(|active| active.start_identity),
        None
    );

    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;

    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(inspected.active_agent.as_deref(), Some(CODEX_HOOK_AGENT));
    assert_eq!(inspected.active_agent_pid, Some(observed.pid));
    assert_eq!(
        inspected.active_agent_session_id.as_deref(),
        Some("hook-native-id")
    );
    assert_eq!(
        inspected.active_agent_session_path.as_deref(),
        Some("/work/hook-native.jsonl")
    );
    assert_eq!(inspected.activity, Some(AgentActivity::Working));
    assert_eq!(inspected.state_source, StateSource::Report);
    {
        let sessions = registry.inner.sessions.lock().await;
        let entry = &sessions[&created.id];
        assert_eq!(
            entry
                .active_agent
                .as_ref()
                .and_then(|active| active.start_identity),
            Some(observed.start_identity.get())
        );
        assert_eq!(
            entry
                .last_agent_report
                .as_ref()
                .and_then(|active| active.start_identity),
            Some(observed.start_identity.get())
        );
    };

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn transient_foreground_error_preserves_last_known_claim() {
    let (registry, inspector, created) = mock_procwatch_registry("foreground-error").await;
    inspector.set_descendants(created.pid, vec![codex_fact(600, created.pid)]);
    inspector.set_foreground_group(created.pid, Some(600));
    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;
    assert_eq!(
        registry
            .inspect(&created.id)
            .await
            .expect("initial inspect")
            .active_agent_pid,
        Some(600)
    );

    inspector.fail_foreground_with(std::io::ErrorKind::PermissionDenied);
    inspector.set_descendants(created.pid, vec![codex_fact(610, created.pid)]);
    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;

    let preserved = registry
        .inspect(&created.id)
        .await
        .expect("preserved inspect");
    assert_eq!(preserved.active_agent.as_deref(), Some("codex"));
    assert_eq!(preserved.active_agent_pid, Some(600));
    assert_eq!(
        registry.inner.sessions.lock().await[&created.id].foreground_process_group,
        Some(600)
    );

    inspector.clear_foreground_error();
    inspector.set_foreground_group(created.pid, Some(610));
    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;
    let recovered = registry
        .inspect(&created.id)
        .await
        .expect("recovered inspect");
    assert_eq!(recovered.active_agent_pid, Some(610));

    let _ = registry.stop(&created.id).await;
}

fn title_activity(config: &DetectorConfig, title: &str) -> Option<AgentActivity> {
    config
        .manifest
        .as_ref()?
        .match_context(&MatchContext::default().with_region_text(ManifestRegion::OscTitle, title))
        .map(|matched| matched.activity)
}

fn pty_command<'a>(program: &str, args: impl IntoIterator<Item = &'a str>) -> LaunchCommand {
    LaunchCommand {
        program: program.to_owned(),
        args: args.into_iter().map(str::to_owned).collect(),
        env: crate::agent::LaunchEnv::default(),
        cwd: crate::test_support::thread_scoped_dir("pohunek-cwd-"),
        cols: 80,
        rows: 24,
    }
}

fn parse_env_dump(text: &str) -> std::collections::HashMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

fn pohunek_env_keys(env: &std::collections::HashMap<String, String>) -> Vec<String> {
    let mut keys: Vec<String> = env
        .keys()
        .filter(|key| key.starts_with("POHUNEK_"))
        .cloned()
        .collect();
    keys.sort();
    keys
}

async fn next_session_updated(rx: &mut tokio::sync::broadcast::Receiver<Event>) -> SessionInfo {
    let event = guard("the session_updated event", async {
        loop {
            let event = rx.recv().await.expect("receive session event");
            if event.event() == protocol::event::SESSION_UPDATED {
                break event;
            }
        }
    })
    .await;
    serde_json::from_value(event.payload()["session"].clone()).expect("session info payload")
}

async fn next_session_removed(rx: &mut tokio::sync::broadcast::Receiver<Event>) -> SessionInfo {
    let event = guard("the session_removed event", async {
        loop {
            let event = rx.recv().await.expect("receive session event");
            if event.event() == protocol::event::SESSION_REMOVED {
                break event;
            }
        }
    })
    .await;
    serde_json::from_value(event.payload()["session"].clone()).expect("session info payload")
}

async fn wait_for_cwd_source(
    registry: &SessionRegistry,
    id: &SessionId,
    cwd: &std::path::Path,
    source: CwdSource,
) -> SessionInfo {
    wait_until(
        &format!("cwd {} from {source:?}", cwd.display()),
        || async {
            let info = registry.inspect(id).await.expect("inspect session");
            (info.cwd == cwd && info.cwd_source == Some(source)).then_some(info)
        },
    )
    .await
}

/// Run git in `dir`, asserting success (test helper for the worktree path).
fn git_in(dir: &std::path::Path, args: &[&str]) {
    let output = pohunek_test_support::process_env::command("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Initialize a throwaway git repo on `main` with one commit, for the
/// worktree-binding path in `create`.
fn init_git_repo(tag: &str) -> PathBuf {
    let dir = temp_store_path(tag)
        .parent()
        .expect("store parent")
        .join("repo");
    std::fs::create_dir_all(&dir).expect("create repo dir");
    let init = pohunek_test_support::process_env::command("git")
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
        .arg(&dir)
        .output()
        .expect("git init");
    assert!(init.status.success(), "git init failed");
    git_in(&dir, &["config", "user.email", "test@example.com"]);
    git_in(&dir, &["config", "user.name", "Test"]);
    git_in(&dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("README.md"), "init\n").expect("write README");
    git_in(&dir, &["add", "."]);
    git_in(&dir, &["commit", "-q", "-m", "init"]);
    dir
}

/// Initialize a throwaway **bare** repo (no working tree) carrying a commit and
/// HEAD, for the bare-project paths. A `--bare` clone of a normal repo gives a
/// bare repo that still has a `main` branch, so `git worktree add` off it works.
fn init_bare_git_repo(tag: &str) -> PathBuf {
    let source = init_git_repo(&format!("{tag}-src"));
    let bare = temp_store_path(tag)
        .parent()
        .expect("store parent")
        .join("bare.git");
    let clone = pohunek_test_support::process_env::command("git")
        .args(["clone", "--bare", "-q"])
        .arg(&source)
        .arg(&bare)
        .output()
        .expect("git clone --bare");
    assert!(
        clone.status.success(),
        "git clone --bare failed: {}",
        String::from_utf8_lossy(&clone.stderr)
    );
    bare
}

#[tokio::test]
async fn session_new_metadata_is_validated_and_exposed() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let expected = metadata(&[("owner", "cli"), ("ticket", "DMD-1356")]);

    let created = registry
        .create(SessionNewParams {
            metadata: expected.clone(),
            ..params()
        })
        .await
        .expect("create session with metadata");
    assert_eq!(created.metadata, expected);
    assert_eq!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect")
            .metadata,
        expected
    );
    let listed = registry.list().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].metadata, expected);

    let invalid: BTreeMap<String, String> = (0..33)
        .map(|index| (format!("key-{index}"), "value".to_owned()))
        .collect();
    let err = registry
        .create(SessionNewParams {
            metadata: invalid,
            ..params()
        })
        .await
        .expect_err("too many metadata keys must be rejected");
    assert_eq!(err.code, "bad_request");
    assert!(
        err.msg.contains("metadata"),
        "metadata validation error must be clear: {err:?}"
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn set_metadata_merges_deletes_updates_timestamp_and_emits_event() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(SessionNewParams {
            metadata: metadata(&[("drop", "soon"), ("keep", "yes"), ("ticket", "old")]),
            ..params()
        })
        .await
        .expect("create session");
    let before_updated_at = created.updated_at.clone();
    let mut events = registry.subscribe();
    let expected = metadata(&[("keep", "yes"), ("owner", "daemon"), ("ticket", "new")]);

    let result = registry
        .set_metadata(
            &created.id,
            metadata_patch(&[
                ("drop", None),
                ("owner", Some("daemon")),
                ("ticket", Some("new")),
            ]),
        )
        .await
        .expect("set metadata");

    assert_eq!(result.session.metadata, expected);
    assert_ne!(result.session.updated_at, before_updated_at);
    assert_eq!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect")
            .metadata,
        expected
    );
    let event_info = next_session_updated(&mut events).await;
    assert_eq!(event_info.id, created.id);
    assert_eq!(event_info.metadata, expected);

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn set_metadata_unknown_session_returns_not_found() {
    let registry = SessionRegistry::default();

    let err = registry
        .set_metadata(&SessionId("s-missing".to_owned()), BTreeMap::new())
        .await
        .expect_err("unknown session id must fail");

    assert_eq!(err.code, "session_not_found");
}

#[tokio::test]
async fn create_with_name_trims_and_stores_it() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(SessionNewParams {
            name: Some("  triage build  ".to_owned()),
            ..params()
        })
        .await
        .expect("create session");

    assert_eq!(created.name.as_deref(), Some("triage build"));

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn rename_sets_then_clears_name_updates_timestamp_and_emits_event() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    assert_eq!(created.name, None);
    let before_updated_at = created.updated_at.clone();
    let mut events = registry.subscribe();

    let renamed = registry
        .rename(&created.id, Some("  feature work  ".to_owned()))
        .await
        .expect("rename session");
    assert_eq!(renamed.session.name.as_deref(), Some("feature work"));
    assert_ne!(renamed.session.updated_at, before_updated_at);
    let event_info = next_session_updated(&mut events).await;
    assert_eq!(event_info.id, created.id);
    assert_eq!(event_info.name.as_deref(), Some("feature work"));

    // An all-whitespace (or `None`) name clears it back to id-only display.
    let cleared = registry
        .rename(&created.id, Some("   ".to_owned()))
        .await
        .expect("clear name");
    assert_eq!(cleared.session.name, None);
    assert_eq!(
        registry.inspect(&created.id).await.expect("inspect").name,
        None
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn rename_rejects_overlong_and_control_character_names() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");

    let too_long = registry
        .rename(&created.id, Some("x".repeat(MAX_SESSION_NAME_BYTES + 1)))
        .await
        .expect_err("overlong name must fail");
    assert_eq!(too_long.code, "bad_request");

    let control = registry
        .rename(&created.id, Some("line1\nline2".to_owned()))
        .await
        .expect_err("control character must fail");
    assert_eq!(control.code, "bad_request");

    // A rejected rename leaves the prior name untouched.
    assert_eq!(
        registry.inspect(&created.id).await.expect("inspect").name,
        None
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn rename_unknown_session_returns_not_found() {
    let registry = SessionRegistry::default();

    let err = registry
        .rename(&SessionId("s-missing".to_owned()), Some("x".to_owned()))
        .await
        .expect_err("unknown session id must fail");

    assert_eq!(err.code, "session_not_found");
}

#[tokio::test]
async fn invalid_metadata_rejected_for_create_or_set_and_set_leaves_session_unchanged() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let mut invalid_create = BTreeMap::new();
    invalid_create.insert("owner".to_owned(), "x".repeat(4097));
    let err = registry
        .create(SessionNewParams {
            metadata: invalid_create,
            ..params()
        })
        .await
        .expect_err("oversized metadata value must be rejected");
    assert_eq!(err.code, "bad_request");
    assert!(registry.list().await.is_empty());

    let created = registry
        .create(SessionNewParams {
            metadata: metadata(&[("owner", "cli")]),
            ..params()
        })
        .await
        .expect("create valid session");
    let original = created.metadata.clone();
    let original_updated_at = created.updated_at.clone();
    let err = registry
        .set_metadata(
            &created.id,
            BTreeMap::from([("x".repeat(65), Some("bad".to_owned()))]),
        )
        .await
        .expect_err("oversized metadata key must be rejected");
    assert_eq!(err.code, "bad_request");

    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(inspected.metadata, original);
    assert_eq!(
        inspected.updated_at, original_updated_at,
        "failed metadata patch must not mutate the session"
    );

    let _ = registry.stop(&created.id).await;

    let key_64_bytes = "é".repeat(32);
    assert_eq!(key_64_bytes.len(), 64);
    let accepted = registry
        .create(SessionNewParams {
            metadata: BTreeMap::from([(key_64_bytes, "byte-boundary".to_owned())]),
            ..params()
        })
        .await
        .expect("64-byte UTF-8 metadata key is accepted");
    let _ = registry.stop(&accepted.id).await;

    let key_66_bytes = "é".repeat(33);
    assert_eq!(key_66_bytes.len(), 66);
    let err = registry
        .create(SessionNewParams {
            metadata: BTreeMap::from([(key_66_bytes, "too-long".to_owned())]),
            ..params()
        })
        .await
        .expect_err("metadata key limit is measured in bytes");
    assert_eq!(err.code, "bad_request");

    let serialized_too_large: BTreeMap<String, String> = (0..super::MAX_SESSION_METADATA_KEYS)
        .map(|index| (format!("key-{index:02}"), "x".repeat(512)))
        .collect();
    assert!(
        serde_json::to_vec(&serialized_too_large)
            .expect("metadata serializes")
            .len()
            > super::MAX_SESSION_METADATA_SERIALIZED_BYTES
    );
    let err = registry
        .create(SessionNewParams {
            metadata: serialized_too_large,
            ..params()
        })
        .await
        .expect_err("metadata serialized size limit must be enforced");
    assert_eq!(err.code, "bad_request");
    assert!(
        err.msg.contains("serialized size"),
        "serialized-size rejection should be clear: {err:?}"
    );
}

#[tokio::test]
async fn failed_launch_rolls_back_the_bound_worktree() {
    // Worktree binding persists the branch checkout before the PTY is spawned.
    // A spawn failure (here: a missing shell program) must roll that back, or
    // the orphan worktree keeps the branch checked out and blocks the next
    // `session.new` on it with `worktree_branch_in_use`.
    let repo = init_git_repo("rollback");
    let store = temp_store_path("rollback");
    let worktree_root = store.parent().expect("store parent").join("worktrees");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new(
            "/nonexistent/pohunek-no-such-shell",
            std::iter::empty::<String>(),
        ),
        store_path: Some(store),
        worktree_root: Some(worktree_root.clone()),
        ..SessionRegistryConfig::default()
    });

    let create_params = SessionNewParams {
        cwd: None,
        name: None,
        repo: Some(repo.clone()),
        branch: Some("feat/x".to_owned()),
        ..params()
    };
    let err = registry
        .create(create_params)
        .await
        .expect_err("launch must fail with a missing shell program");
    // A missing program (ENOENT) at spawn surfaces the precise
    // `agent_binary_missing` diagnostic naming the program, with a recover
    // hint — not the generic `spawn_failed`.
    assert_eq!(err.code, "agent_binary_missing", "got: {err:?}");
    assert!(
        err.msg.contains("pohunek-no-such-shell"),
        "error must name the missing program: {err:?}"
    );
    assert!(
        err.recover.is_some(),
        "missing-binary error carries a hint: {err:?}"
    );

    // The worktree bound before the failed spawn must be gone, so its branch
    // is freed for a retry.
    let leftover: Vec<_> = std::fs::read_dir(&worktree_root)
        .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(
        leftover.is_empty(),
        "a failed launch must leave no orphan worktree under {}: {leftover:?}",
        worktree_root.display()
    );

    // And git no longer holds feat/x in any worktree, so a fresh bind succeeds.
    let listing = pohunek_test_support::process_env::command("git")
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .expect("git worktree list");
    let listing = String::from_utf8_lossy(&listing.stdout);
    assert!(
        !listing.contains("feat/x"),
        "branch checkout must be pruned from git's worktree list: {listing}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn incompatible_hermes_profile_fails_before_session_and_worktree_side_effects() {
    for (case, version_output) in [
        ("missing", None),
        ("wrong", Some("Hermes Agent v0.21.0")),
        ("unparseable", Some("unexpected provider output")),
    ] {
        let repo = init_git_repo(&format!("hermes-policy-{case}"));
        let store_path = temp_store_path(&format!("hermes-policy-{case}"));
        let root = store_path.parent().expect("store parent");
        let worktree_root = root.join("worktrees");
        let marker = root.join("launched");
        let executable = root.join(format!("hermes-{case}"));
        if let Some(version_output) = version_output {
            write_executable(
                &executable,
                &format!(
                    "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf '%s\\n' '{version_output}'; exit 0; fi\ntouch {}\nsleep 30\n",
                    marker.display()
                ),
            );
        }
        let agents_dir = temp_agents_dir_with(
            &format!("hermes-policy-{case}"),
            "hermes-policy",
            &format!(
                "base = \"hermes\"\nprogram = \"{}\"\nargs = [\"chat\"]\n",
                executable.display()
            ),
        );
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: hermetic_shell(),
            agents_dir: Some(agents_dir),
            store_path: Some(store_path.clone()),
            worktree_root: Some(worktree_root.clone()),
            ..SessionRegistryConfig::default()
        });

        let error = registry
            .create(SessionNewParams {
                agent: "hermes-policy".to_owned(),
                cwd: None,
                repo: Some(repo.clone()),
                branch: Some(format!("feat/hermes-{case}")),
                ..params()
            })
            .await
            .expect_err("incompatible Hermes runtime must fail before launch");

        assert_eq!(error.code, "agent_runtime_unsupported");
        assert!(!error.msg.contains(&executable.display().to_string()));
        if let Some(version_output) = version_output {
            assert!(!error.msg.contains(version_output));
        }
        assert!(registry.list().await.is_empty());
        assert!(
            !marker.exists(),
            "Hermes process must not launch for {case}"
        );
        let worktrees = std::fs::read_dir(&worktree_root)
            .map(|entries| entries.filter_map(Result::ok).count())
            .unwrap_or_default();
        assert_eq!(worktrees, 0, "no worktree side effect for {case}");
        assert!(
            crate::store::Store::new(store_path)
                .load_sessions()
                .expect("load sessions")
                .is_empty(),
            "no logical session record for {case}"
        );
        let worktree_listing = pohunek_test_support::process_env::command("git")
            .arg("-C")
            .arg(repo)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .expect("list git worktrees");
        assert!(worktree_listing.status.success());
        assert!(!String::from_utf8_lossy(&worktree_listing.stdout).contains("feat/hermes-"));
    }
}

/// Raw `$SHELL` values the daemon must never launch or persist.
fn invalid_login_shells() -> Vec<Option<String>> {
    vec![
        Some(String::new()),
        Some("s".repeat(crate::agent::host::MAX_ARG_BYTES + 1)),
        Some("/bin/s\nh".to_owned()),
        Some("/bin/s\u{7f}h".to_owned()),
    ]
}

#[test]
fn invalid_login_shell_resolves_to_the_fallback_everywhere() {
    for raw in invalid_login_shells().into_iter().chain([None]) {
        assert_eq!(
            crate::agent::resolve_login_shell(raw.clone()),
            crate::agent::FALLBACK_LOGIN_SHELL
        );
        assert_eq!(
            ShellCommand::from_login_shell(raw.clone()).program,
            crate::agent::FALLBACK_LOGIN_SHELL
        );
        assert_eq!(
            crate::agent::host::RuntimeSource::load(
                &crate::agent::host::BuiltinSource::from_login_shell(raw)
            )
            .expect("built-ins load")
            .iter()
            .find(|definition| definition.runtime_id().as_str() == "shell")
            .expect("shell definition")
            .program()
            .as_str(),
            crate::agent::FALLBACK_LOGIN_SHELL
        );
    }
    assert_eq!(
        crate::agent::resolve_login_shell(Some("/usr/bin/zsh".to_owned())),
        "/usr/bin/zsh"
    );
}

#[tokio::test]
async fn shell_session_launches_the_fallback_for_an_invalid_login_shell() {
    for raw in invalid_login_shells() {
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::from_login_shell(raw),
            ..SessionRegistryConfig::default()
        });
        let info = registry
            .create(params())
            .await
            .expect("a shell session launches the fallback shell");
        assert_eq!(info.agent, "shell");
        registry.stop(&info.id).await.expect("session stops");
    }
}

#[tokio::test]
async fn failed_initial_input_rollback_frees_the_bound_worktree() {
    // A worktree-bound session whose `--input` injection fails must roll the
    // worktree back, not just the PTY: `stop()` alone leaves the checkout in
    // place, blocking the next `session.new` on the branch with
    // `worktree_branch_in_use`. Drive the exact rollback the failed-input
    // branch of `create` performs and assert the branch is freed.
    let repo = init_git_repo("input-rollback");
    let store = temp_store_path("input-rollback");
    let worktree_root = store.parent().expect("store parent").join("worktrees");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        store_path: Some(store),
        worktree_root: Some(worktree_root.clone()),
        ..SessionRegistryConfig::default()
    });

    // A real worktree-bound session (launch succeeds, branch checked out).
    let info = registry
        .create(SessionNewParams {
            cwd: None,
            name: None,
            repo: Some(repo.clone()),
            branch: Some("feat/x".to_owned()),
            ..params()
        })
        .await
        .expect("worktree-bound session is created");
    assert!(
        info.worktree_path.is_some(),
        "session must be worktree-bound for this test: {info:?}"
    );

    registry.rollback_failed_initial_input(&info.id).await;
    assert!(
        registry.list().await.is_empty(),
        "the rolled-back session is removed"
    );

    // The worktree bound for this session must be gone so its branch is free.
    let leftover: Vec<_> = std::fs::read_dir(&worktree_root)
        .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(
        leftover.is_empty(),
        "rollback must leave no orphan worktree under {}: {leftover:?}",
        worktree_root.display()
    );

    // git no longer holds feat/x in any worktree, so a fresh bind succeeds.
    let listing = pohunek_test_support::process_env::command("git")
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .expect("git worktree list");
    let listing = String::from_utf8_lossy(&listing.stdout);
    assert!(
        !listing.contains("feat/x"),
        "branch checkout must be pruned so a fresh bind succeeds: {listing}"
    );
}

/// A registry with persistence + worktree binding configured, using the
/// default shell so a launch actually succeeds (for the project-wiring tests).
fn project_registry(tag: &str) -> (SessionRegistry, PathBuf) {
    let store = temp_store_path(tag);
    let worktree_root = store.parent().expect("store parent").join("worktrees");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        store_path: Some(store),
        worktree_root: Some(worktree_root),
        ..SessionRegistryConfig::default()
    });
    let repo = init_git_repo(tag);
    (registry, repo)
}

/// A project registry whose procwatch never reads a session cwd.
///
/// The OSC 7 hints these tests send do not move the real shell, so a host
/// procwatch scan would report the shell's real cwd between two hints and
/// race the assertions. With no scripted cwd, only the hints move a session.
fn hint_registry(tag: &str) -> (SessionRegistry, PathBuf) {
    let store = temp_store_path(tag);
    let worktree_root = store.parent().expect("store parent").join("worktrees");
    let registry = SessionRegistry::new_with_inspector(
        SessionRegistryConfig {
            shell_command: hermetic_shell(),
            store_path: Some(store),
            worktree_root: Some(worktree_root),
            ..SessionRegistryConfig::default()
        },
        Arc::new(MockInspector::default()),
    );
    let repo = init_git_repo(tag);
    (registry, repo)
}

#[tokio::test]
async fn session_new_auto_registers_project_from_cwd_and_stamps_ids() {
    // The first observable change (M3): starting a session inside a git work
    // tree with no flags runs in-place and silently records an Auto project,
    // stamping the session's project_id / is_linked_worktree.
    let (registry, repo) = project_registry("auto-register");
    let info = registry
        .create(SessionNewParams {
            cwd: Some(repo.clone()),
            ..params()
        })
        .await
        .expect("session is created in the repo");

    let canonical_repo = std::fs::canonicalize(&repo).expect("canonical repo");
    assert_eq!(info.cwd, canonical_repo, "in-place runs in the checkout");
    assert_eq!(info.worktree_path, None, "in-place binds no worktree");
    assert_eq!(info.is_linked_worktree, Some(false), "the main checkout");
    let project_id = info.project_id.clone().expect("a project was stamped");

    let projects = registry
        .projects()
        .expect("projects configured")
        .store()
        .load_projects()
        .expect("load projects");
    assert_eq!(projects.len(), 1, "exactly one project auto-registered");
    assert_eq!(projects[0].source, ProjectSource::Auto);
    assert_eq!(
        projects[0].id(),
        project_id,
        "session id matches the record"
    );
    assert_eq!(
        projects[0].git_common_dir,
        std::fs::canonicalize(repo.join(".git")).expect("canonical .git")
    );
}

#[tokio::test]
async fn in_place_session_on_a_bare_project_is_refused() {
    // A bare repo has no working tree; an in-place agent would land in the bare
    // git dir. The default (no --branch) start must be refused with a message
    // steering the operator to --branch, not silently launched in the git dir.
    let (registry, _repo) = project_registry("bare-inplace");
    let bare = init_bare_git_repo("bare-inplace");

    let err = registry
        .create(SessionNewParams {
            cwd: Some(bare.clone()),
            ..params()
        })
        .await
        .expect_err("in-place on a bare repo must be refused");
    assert!(
        err.msg.contains("bare repository") && err.msg.contains("--branch"),
        "error must explain the bare repo and steer to --branch: {err:?}"
    );
    // Nothing was launched.
    assert!(
        registry.list().await.is_empty(),
        "no session is created for a refused in-place bare start"
    );
}

#[tokio::test]
async fn worktree_session_on_a_bare_project_is_allowed() {
    // The steer in `in_place_session_on_a_bare_project_is_refused` is only valid
    // if --branch actually works on a bare repo: a worktree is added off it.
    let (registry, _repo) = project_registry("bare-worktree");
    let bare = init_bare_git_repo("bare-worktree");

    let info = registry
        .create(SessionNewParams {
            cwd: Some(bare.clone()),
            name: None,
            branch: Some("feat/x".to_owned()),
            base_branch: Some("main".to_owned()),
            ..params()
        })
        .await
        .expect("a worktree session is allowed on a bare repo");
    assert!(
        info.worktree_path.is_some(),
        "a worktree was bound off the bare repo: {info:?}"
    );
    assert_eq!(info.branch.as_deref(), Some("feat/x"));
}

#[tokio::test]
async fn session_new_in_a_non_git_cwd_records_no_project() {
    // A plain shell in a non-git directory: no project, no stamping, today's
    // behavior unchanged.
    let (registry, _repo) = project_registry("non-git");
    let non_git = crate::test_support::thread_scoped_dir("pohunek-nongit-");

    let info = registry
        .create(SessionNewParams {
            cwd: Some(non_git.clone()),
            ..params()
        })
        .await
        .expect("plain shell session is created");

    assert_eq!(info.project_id, None, "no git ⇒ no project");
    assert_eq!(info.is_linked_worktree, None);
    assert_eq!(info.worktree_path, None);
    assert!(
        registry
            .projects()
            .expect("projects configured")
            .store()
            .load_projects()
            .expect("load")
            .is_empty(),
        "a non-git directory must register nothing"
    );
}

#[tokio::test]
async fn session_new_with_project_ref_binds_the_main_checkout_in_place() {
    // Resolve a project by its id reference (the only remote-capable option):
    // an in-place session launches in the project's main checkout.
    let (registry, repo) = project_registry("by-ref");
    // Auto-register by starting once in the repo, then reference it by id.
    let first = registry
        .create(SessionNewParams {
            cwd: Some(repo.clone()),
            ..params()
        })
        .await
        .expect("first session auto-registers the project");
    let project_id = first.project_id.clone().expect("project stamped");

    let info = registry
        .create(SessionNewParams {
            cwd: None,
            name: None,
            project: Some(project_id.clone()),
            ..params()
        })
        .await
        .expect("session created from a --project reference");

    assert_eq!(info.project_id.as_deref(), Some(project_id.as_str()));
    assert_eq!(info.worktree_path, None, "no --branch ⇒ in-place");
    assert_eq!(info.is_linked_worktree, Some(false));
    assert_eq!(
        info.cwd,
        std::fs::canonicalize(&repo).expect("canonical repo"),
        "in-place runs in the project's main checkout"
    );
}

#[tokio::test]
async fn session_new_with_unknown_project_ref_is_rejected() {
    let (registry, _repo) = project_registry("unknown-ref");
    let err = registry
        .create(SessionNewParams {
            cwd: None,
            name: None,
            project: Some("does-not-exist".to_owned()),
            ..params()
        })
        .await
        .expect_err("an unknown project reference must error");
    assert_eq!(err.code, "project_not_found", "got: {err:?}");
}

#[tokio::test]
async fn session_new_branch_with_detected_project_binds_worktree_carrying_project_id() {
    // `--branch` in a detected project builds a worktree-per-session off the
    // project's repo; the worktree's binding carries the project id so prune /
    // `project show` can find pohunek's own worktrees later (M5).
    let (registry, repo) = project_registry("wt-project");
    let info = registry
        .create(SessionNewParams {
            cwd: Some(repo.clone()),
            name: None,
            branch: Some("feat/x".to_owned()),
            ..params()
        })
        .await
        .expect("worktree session created from the detected project");

    assert!(info.worktree_path.is_some(), "--branch binds a worktree");
    assert_eq!(info.is_linked_worktree, Some(true));
    let project_id = info.project_id.clone().expect("project stamped on session");

    let binding = registry
        .projects()
        .expect("projects configured")
        .store()
        .load_worktrees()
        .expect("load worktrees")
        .into_iter()
        .find(|b| b.session_id == info.id.0)
        .expect("this session has a worktree binding");
    assert_eq!(
        binding.project_id.as_deref(),
        Some(project_id.as_str()),
        "the worktree binding must carry the project id"
    );
}

#[tokio::test]
async fn cwd_hint_remaps_between_registered_worktrees() {
    let (registry, repo) = hint_registry("cwd-remap-worktrees");
    let first = registry
        .create(SessionNewParams {
            cwd: Some(repo.clone()),
            name: None,
            branch: Some("feat/a".to_owned()),
            ..params()
        })
        .await
        .expect("first worktree session");
    let second = registry
        .create(SessionNewParams {
            cwd: Some(repo),
            name: None,
            branch: Some("feat/b".to_owned()),
            ..params()
        })
        .await
        .expect("second worktree session");

    let first_path = first.worktree_path.clone().expect("first worktree");
    let second_path = second.worktree_path.clone().expect("second worktree");
    let second_nested = second_path.join("nested");
    fs::create_dir_all(&second_nested).expect("create nested cwd");

    registry
        .record_cwd_hint(&first.id, second_nested.display().to_string())
        .await;
    let moved = registry.inspect(&first.id).await.expect("inspect moved");

    assert_eq!(moved.cwd, second_nested);
    assert_eq!(moved.cwd_source, Some(CwdSource::Osc7));
    assert_eq!(moved.worktree_path.as_deref(), Some(second_path.as_path()));
    assert_eq!(moved.branch.as_deref(), Some("feat/b"));
    assert_eq!(moved.is_linked_worktree, Some(true));
    assert_eq!(moved.project_id, first.project_id);

    registry
        .record_cwd_hint(&first.id, first_path.display().to_string())
        .await;
    let restored = registry.inspect(&first.id).await.expect("inspect restored");

    assert_eq!(restored.cwd, first_path);
    assert_eq!(restored.cwd_source, Some(CwdSource::Osc7));
    assert_eq!(restored.worktree_path, first.worktree_path);
    assert_eq!(restored.branch.as_deref(), Some("feat/a"));
    assert_eq!(restored.is_linked_worktree, Some(true));
    assert_eq!(restored.project_id, first.project_id);

    let _ = registry.stop(&first.id).await;
    let _ = registry.stop(&second.id).await;
}

#[tokio::test]
async fn older_cwd_evidence_never_replaces_newer_evidence() {
    let (registry, _inspector, created) = mock_procwatch_registry("cwd-evidence-order").await;
    let launched = created.cwd.clone();
    let hinted = temp_dir("cwd-evidence-hint");
    // A procwatch scan read the launch cwd, then an OSC 7 hint landed before
    // the scan got to apply its result.
    let scanned = Instant::now();
    let hinted_at = scanned + Duration::from_millis(1);

    registry
        .apply_cwd_change(
            &created.id,
            hinted.clone(),
            CwdSource::Osc7,
            None,
            hinted_at,
        )
        .await;
    registry
        .apply_cwd_change(
            &created.id,
            launched.clone(),
            CwdSource::Procwatch,
            None,
            scanned,
        )
        .await;
    let kept = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(kept.cwd, hinted);
    assert_eq!(kept.cwd_source, Some(CwdSource::Osc7));

    // Newer evidence for the same cwd keeps the source that established it.
    let confirmed_at = hinted_at + Duration::from_millis(1);
    registry
        .apply_cwd_change(
            &created.id,
            hinted.clone(),
            CwdSource::Procwatch,
            None,
            confirmed_at,
        )
        .await;
    let confirmed = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(confirmed.cwd, hinted);
    assert_eq!(confirmed.cwd_source, Some(CwdSource::Osc7));

    // Newer evidence of a different cwd moves the session.
    registry
        .apply_cwd_change(
            &created.id,
            launched.clone(),
            CwdSource::Procwatch,
            None,
            confirmed_at + Duration::from_millis(1),
        )
        .await;
    let moved = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(moved.cwd, launched);
    assert_eq!(moved.cwd_source, Some(CwdSource::Procwatch));

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn cwd_hint_into_an_unregistered_repo_registers_no_project() {
    // Regression: cwd hints (OSC 7 / procwatch focus) used to upsert an Auto
    // project record for whatever repo the observed cwd landed in, so a repo a
    // watched process merely sat in — e.g. a throwaway fixture repo created by
    // a nested test-suite daemon — permanently polluted the registry. Hints
    // must only *derive* the association; registration stays on session.new
    // and explicit `project add`.
    let (registry, repo) = hint_registry("hint-no-register");
    let session = registry
        .create(SessionNewParams {
            cwd: Some(repo),
            ..params()
        })
        .await
        .expect("session in the repo");

    let other_repo = init_git_repo("hint-no-register-other");
    registry
        .record_cwd_hint(&session.id, other_repo.display().to_string())
        .await;
    let moved = registry.inspect(&session.id).await.expect("inspect moved");

    assert_eq!(moved.cwd, other_repo);
    assert_eq!(moved.cwd_source, Some(CwdSource::Osc7));
    let other_git_common_dir =
        std::fs::canonicalize(other_repo.join(".git")).expect("canonical other .git");
    assert_eq!(
        moved.project_id.as_deref(),
        Some(project_id(&other_git_common_dir).as_str()),
        "the association still carries the stable derived project id"
    );

    let projects = registry
        .projects()
        .expect("projects configured")
        .store()
        .load_projects()
        .expect("load projects");
    assert_eq!(
        projects.len(),
        1,
        "only the session.new repo is registered; the hinted repo must not be: {projects:?}"
    );
    assert_eq!(projects[0].id(), session.project_id.expect("stamped"));

    let _ = registry.stop(&session.id).await;
}

#[tokio::test]
async fn session_new_with_project_ref_bumps_last_used_at() {
    // The data model defines last_used_at as bumped on each session start; the
    // --project reference path must do that too, not only auto-detection.
    let (registry, repo) = project_registry("touch");
    let first = registry
        .create(SessionNewParams {
            cwd: Some(repo.clone()),
            ..params()
        })
        .await
        .expect("first session auto-registers");
    let project_id = first.project_id.clone().expect("project stamped");
    let projects = registry.projects().expect("projects");
    let store = projects.store();

    // Backdate last_used_at so the bump is unambiguously observable.
    let mut record = store
        .load_projects()
        .expect("load")
        .into_iter()
        .next()
        .expect("one project");
    record.last_used_at = "2000-01-01T00:00:00Z".to_owned();
    store.record_project(&record).expect("backdate");

    registry
        .create(SessionNewParams {
            cwd: None,
            name: None,
            project: Some(project_id),
            ..params()
        })
        .await
        .expect("session created by --project reference");

    let after = store
        .load_projects()
        .expect("reload")
        .into_iter()
        .next()
        .expect("one project");
    assert_ne!(
        after.last_used_at, "2000-01-01T00:00:00Z",
        "a --project reference must bump last_used_at"
    );
}

#[tokio::test]
async fn session_new_with_explicit_non_git_repo_errors() {
    // An explicitly named --repo that is not a git work tree must error, not
    // silently launch a plain shell somewhere else (no silent defaults).
    let (registry, _repo) = project_registry("explicit-nonrepo");
    let nonrepo = crate::test_support::thread_scoped_dir("pohunek-nonrepo-");

    let err = registry
        .create(SessionNewParams {
            cwd: None,
            name: None,
            repo: Some(nonrepo),
            ..params()
        })
        .await
        .expect_err("an explicit non-git --repo must error");
    assert_eq!(err.code, "not_a_git_repo", "got: {err:?}");
}

#[tokio::test]
async fn session_new_rejects_project_and_repo_together() {
    // --project and --repo both name the target repo; accepting both would
    // persist an incoherent binding, so the daemon rejects the combination.
    let (registry, repo) = project_registry("mutual-exclusion");
    let err = registry
        .create(SessionNewParams {
            cwd: None,
            name: None,
            project: Some("anything".to_owned()),
            repo: Some(repo),
            ..params()
        })
        .await
        .expect_err("--project and --repo together must be rejected");
    assert!(err.msg.contains("mutually exclusive"), "got: {err:?}");
}

#[tokio::test]
async fn remove_project_with_prune_removes_owned_worktrees_and_forgets_the_record() {
    let (registry, repo) = project_registry("prune");
    // A worktree session: its binding carries the project id.
    let info = registry
        .create(SessionNewParams {
            cwd: Some(repo.clone()),
            name: None,
            branch: Some("feat/x".to_owned()),
            ..params()
        })
        .await
        .expect("worktree session created");
    let worktree = info.worktree_path.clone().expect("worktree path");
    let project_id = info.project_id.clone().expect("project stamped");
    assert!(worktree.exists());
    // Stop the session first; its worktree binding is intentionally kept.
    registry.stop(&info.id).await.expect("stop session");

    let result = registry
        .remove_project(&project_id, true)
        .await
        .expect("remove with prune");
    assert!(result.removed, "the project record was removed");
    assert_eq!(result.pruned_worktrees, 1, "the owned worktree was pruned");
    assert!(!worktree.exists(), "pruned worktree directory is gone");
    assert!(
        registry
            .projects()
            .expect("projects")
            .store()
            .load_projects()
            .expect("load")
            .is_empty(),
        "the project record is forgotten"
    );
}

#[tokio::test]
async fn remove_project_prune_skips_a_worktree_with_a_live_session() {
    // A worktree a RUNNING session is using must not be pruned out from under
    // it; it is skipped and reported. Because a worktree was skipped, the
    // record is KEPT (removed = false) so its surviving binding keeps pointing
    // at a real project (Option (b)); a later `rm` forgets it once idle.
    let (registry, repo) = project_registry("prune-skip");
    let info = registry
        .create(SessionNewParams {
            cwd: Some(repo.clone()),
            name: None,
            branch: Some("feat/x".to_owned()),
            ..params()
        })
        .await
        .expect("worktree session created");
    let worktree = info.worktree_path.clone().expect("worktree path");
    let project_id = info.project_id.clone().expect("project stamped");
    // The session is left RUNNING (not stopped) — it is live in the worktree.

    let result = registry
        .remove_project(&project_id, true)
        .await
        .expect("remove with prune");
    assert!(
        !result.removed,
        "the record is kept while a live worktree remains"
    );
    assert_eq!(
        result.pruned_worktrees, 0,
        "the live worktree is not pruned"
    );
    assert_eq!(
        result.skipped_worktrees,
        vec![info.id.0.clone()],
        "the live session is reported as skipped"
    );
    assert!(
        worktree.exists(),
        "a live session's worktree is left on disk"
    );
    assert!(
        !registry
            .projects()
            .expect("projects")
            .store()
            .load_projects()
            .expect("load")
            .is_empty(),
        "the record stays so the skipped worktree's binding is not dangling"
    );
}

#[tokio::test]
async fn remove_project_without_prune_leaves_worktrees_intact() {
    let (registry, repo) = project_registry("no-prune");
    let info = registry
        .create(SessionNewParams {
            cwd: Some(repo.clone()),
            name: None,
            branch: Some("feat/x".to_owned()),
            ..params()
        })
        .await
        .expect("worktree session created");
    let worktree = info.worktree_path.clone().expect("worktree path");
    let project_id = info.project_id.clone().expect("project stamped");
    registry.stop(&info.id).await.expect("stop session");

    let result = registry
        .remove_project(&project_id, false)
        .await
        .expect("remove without prune");
    assert!(result.removed);
    assert_eq!(
        result.pruned_worktrees, 0,
        "nothing pruned without the flag"
    );
    assert!(
        worktree.exists(),
        "a plain rm must leave the worktree on disk"
    );
}

#[tokio::test]
async fn remove_worktree_removes_an_owned_idle_worktree() {
    let (registry, repo) = project_registry("wt-remove");
    let info = registry
        .create(SessionNewParams {
            cwd: Some(repo.clone()),
            name: None,
            branch: Some("feat/x".to_owned()),
            ..params()
        })
        .await
        .expect("worktree session created");
    let worktree = info.worktree_path.clone().expect("worktree path");
    assert!(worktree.exists());
    // Stop the session so it is terminal; its binding (ownership proof) stays.
    registry.stop(&info.id).await.expect("stop session");

    let result = registry
        .remove_worktree(&worktree)
        .await
        .expect("remove owned worktree");
    assert!(result.removed, "the owned worktree was removed");
    assert!(!worktree.exists(), "the worktree directory is gone");
}

#[tokio::test]
async fn remove_worktree_refuses_a_live_session() {
    let (registry, repo) = project_registry("wt-remove-live");
    let info = registry
        .create(SessionNewParams {
            cwd: Some(repo.clone()),
            name: None,
            branch: Some("feat/x".to_owned()),
            ..params()
        })
        .await
        .expect("worktree session created");
    let worktree = info.worktree_path.clone().expect("worktree path");
    // The session is left RUNNING — it is live in the worktree.

    let err = registry
        .remove_worktree(&worktree)
        .await
        .expect_err("a live worktree is refused");
    assert_eq!(err, protocol::ProtocolError::worktree_in_use());
    assert!(
        worktree.exists(),
        "a live session's worktree is left on disk"
    );
}

#[tokio::test]
async fn remove_worktree_refuses_an_unowned_path() {
    // The main checkout has no worktree binding, so it is not pohunek-owned and
    // must be refused rather than removed.
    let (registry, repo) = project_registry("wt-remove-unowned");
    let err = registry
        .remove_worktree(&repo)
        .await
        .expect_err("an unowned path is refused");
    assert_eq!(err.code, "worktree_not_owned");
    assert!(repo.exists(), "the main checkout is untouched");
}

#[tokio::test]
async fn missing_program_spawn_returns_agent_binary_missing() {
    // A plain shell session whose program does not exist fails at the PTY
    // spawn (ENOENT). That must map to the typed `agent_binary_missing` error
    // naming the program and carrying a recover hint, not `spawn_failed`.
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new(
            "/nonexistent/pohunek-missing-program",
            std::iter::empty::<String>(),
        ),
        ..SessionRegistryConfig::default()
    });

    let err = registry
        .create(params())
        .await
        .expect_err("missing program must fail to spawn");

    assert_eq!(err.code, "agent_binary_missing", "got: {err:?}");
    assert!(
        err.msg.contains("pohunek-missing-program"),
        "error must name the missing program: {err:?}"
    );
    assert!(err.recover.is_some(), "must carry a recover hint: {err:?}");
}

#[tokio::test]
async fn detects_successful_process_exit() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "exit 0"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });

    let created = registry.create(params()).await.expect("create session");
    let exit = registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("session exits");

    assert_eq!(exit.state, SessionState::Done);
    assert_eq!(exit.exit_code, Some(0));
}

#[tokio::test]
async fn session_child_sees_the_fixture_home_and_not_the_developer_environment() {
    let root = temp_dir("session-child-home");
    let cwd = temp_dir("session-child-home-cwd");
    let report = root.join("child-env.txt");
    let crate::runtime::EnvironmentSource::Fixed(fixture) =
        crate::test_support::thread_environment_source()
    else {
        panic!("a test fixture supplies an explicit environment");
    };
    let fixture_home = fixture
        .get(std::ffi::OsStr::new("HOME"))
        .expect("the fixture environment pins HOME")
        .clone();
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new(
            "/bin/sh",
            [
                "-c",
                &format!(
                    "printf 'HOME=%s\\nSSH_AUTH_SOCK=%s\\nDISPLAY=%s\\n' \
                     \"$HOME\" \"${{SSH_AUTH_SOCK-unset}}\" \"${{DISPLAY-unset}}\" > '{}'; sleep 30",
                    report.display()
                ),
            ],
        ),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(SessionNewParams {
            cwd: Some(cwd),
            ..params()
        })
        .await
        .expect("create session");

    wait_for_file_contains(&report, "DISPLAY=").await;
    let child = parse_env_dump(&fs::read_to_string(&report).expect("read the child report"));
    assert_eq!(
        child.get("HOME").map(String::as_str),
        fixture_home.to_str(),
        "the session child must see the fixture HOME"
    );
    if let Some(process_home) = std::env::var_os("HOME") {
        assert_ne!(
            child.get("HOME").map(std::ffi::OsStr::new),
            Some(process_home.as_os_str()),
            "the session child must not see the developer's HOME"
        );
    }
    assert_eq!(
        child.get("SSH_AUTH_SOCK").map(String::as_str),
        Some("unset")
    );
    assert_eq!(child.get("DISPLAY").map(String::as_str), Some("unset"));

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn session_start_hook_runs_after_spawn_without_blocking_create() {
    let config_dir = temp_dir("session-start-config");
    let cwd = temp_dir("session-start-cwd");
    let marker = config_dir.join("session-start.marker");
    write_host_hook(
            &config_dir,
            "session-start",
            &format!(
                "#!/bin/sh\nprintf '%s:%s:%s\\n' \"$POHUNEK_HOOK_EVENT\" \"$POHUNEK_SESSION_ID\" \"$POHUNEK_AGENT\" >> {}\n",
                marker.display()
            ),
        );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        config_dir: Some(config_dir.clone()),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(SessionNewParams {
            cwd: Some(cwd),
            ..params()
        })
        .await
        .expect("create session returns while hook runs best-effort");

    let contents =
        wait_for_file_contains(&marker, &format!("session-start:{}:shell", created.id.0)).await;
    assert_eq!(contents.lines().count(), 1, "session-start fires once");

    let _ = registry.stop(&created.id).await;
}

/// Creates a FIFO at `path` and opens it for reading and writing.
///
/// A hook that opens the FIFO for reading and then does `read _` blocks until
/// the test releases it with [`release_hook_gate`]. The test's own handle is a
/// writer for as long as it lives, so the hook never sees end-of-file while the
/// test holds it, and opening read-write never blocks. Keep the handle alive
/// until the hook has acknowledged the release: a FIFO with no open descriptor
/// discards its buffered data, and a later reader-open would block.
pub(super) fn hook_gate(path: &std::path::Path) -> fs::File {
    nix::unistd::mkfifo(path, nix::sys::stat::Mode::S_IRWXU).expect("create hook gate fifo");
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open hook gate")
}

/// Writes one line into the gate so a hook parked in `read _` on it continues.
fn release_hook_gate(gate: &fs::File) {
    let mut writer = gate;
    writer
        .write_all(b"release\n")
        .expect("write the hook gate release line");
}

#[tokio::test]
async fn session_start_hook_runs_detached_from_create() {
    let config_dir = temp_dir("session-start-detached-config");
    let cwd = temp_dir("session-start-detached-cwd");
    let gate_path = config_dir.join("hook.gate");
    let started = config_dir.join("hook.started");
    let released = config_dir.join("hook.released");
    let gate = hook_gate(&gate_path);
    write_host_hook(
        &config_dir,
        "session-start",
        &format!(
            "#!/bin/sh\nexec 3< '{}'\necho started > '{}'\nread _ <&3\necho released > '{}'\n",
            gate_path.display(),
            started.display(),
            released.display(),
        ),
    );
    // The registry keeps its default hook timeout, so the gated hook cannot be
    // terminated before the assertions below observe it.
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        config_dir: Some(config_dir),
        ..SessionRegistryConfig::default()
    });

    // The hook opens the gate before it writes `started`, so `started` implies
    // it holds a reader on a FIFO whose only writer is the test's handle and
    // that carries no data. It cannot pass `read` until the test writes a line,
    // so a `create` that waited for it would not return. `create` runs in its
    // own task so that the hang guard can release the gate, join the task and
    // fail with a diagnostic instead of waiting for the hook timeout.
    let mut creator = tokio::spawn({
        let registry = registry.clone();
        async move {
            registry
                .create(SessionNewParams {
                    cwd: Some(cwd),
                    ..params()
                })
                .await
        }
    });
    let outcome = tokio::time::timeout(pohunek_test_support::wait::HANG_GUARD, &mut creator).await;
    let Ok(joined) = outcome else {
        release_hook_gate(&gate);
        let late = pohunek_test_support::wait::guard(
            "create to return after the gate was released",
            creator,
        )
        .await
        .expect("create task joins after the release");
        if let Ok(late) = late {
            let _ = registry.stop(&late.id).await;
        }
        panic!(
            "create waited for the session-start hook past the hang guard of {:?}",
            pohunek_test_support::wait::HANG_GUARD
        );
    };
    let created = joined
        .expect("create task joins")
        .expect("create session returns while the session-start hook is still running");

    pohunek_test_support::wait::wait_until("the hook to report started", || async {
        fs::read_to_string(&started)
            .ok()
            .filter(|contents| contents.contains("started"))
    })
    .await;
    assert!(
        !released.exists(),
        "the hook is parked on its gate after create returned"
    );
    release_hook_gate(&gate);
    pohunek_test_support::wait::wait_until("the hook to report released", || async {
        fs::read_to_string(&released)
            .ok()
            .filter(|contents| contents.contains("released"))
    })
    .await;
    drop(gate);

    let _ = registry.stop(&created.id).await;
}

/// Bounds how long [`session_start_hook_is_terminated_after_hook_timeout`] waits
/// for the runner to terminate the gated hook; the test asserts no duration.
const SESSION_HOOK_TIMEOUT_UNDER_TEST: Duration = Duration::from_millis(50);

#[tokio::test]
async fn session_start_hook_is_terminated_after_hook_timeout() {
    let config_dir = temp_dir("session-start-timeout-config");
    let cwd = temp_dir("session-start-timeout-cwd");
    let gate_path = config_dir.join("hook.gate");
    let gate = hook_gate(&gate_path);
    write_host_hook(
        &config_dir,
        "session-start",
        &format!("#!/bin/sh\nread _ < '{}'\n", gate_path.display()),
    );
    let context = crate::worktree::HookContext {
        session_id: "hook-timeout".to_owned(),
        project_id: None,
        agent: "shell".to_owned(),
        repo: None,
        worktree: None,
        branch: None,
        base_branch: None,
        stop_reason: None,
        activity: None,
    };

    // The hook is parked on the gate until the test releases it, so `run_hook`
    // returns only if it enforces the timeout.
    let mut runner = tokio::task::spawn_blocking(move || {
        let mut warnings = Vec::new();
        crate::worktree::run_hook(
            crate::worktree::HookEvent::SessionStart,
            &cwd,
            &context,
            SESSION_HOOK_TIMEOUT_UNDER_TEST,
            Some(&config_dir),
            &mut warnings,
        );
        warnings
    });
    // Without timeout enforcement the runner blocks for as long as the hook
    // does, so the hang guard releases the gate to let it finish before the
    // test fails with an explicit message.
    let outcome = tokio::time::timeout(pohunek_test_support::wait::HANG_GUARD, &mut runner).await;
    let Ok(joined) = outcome else {
        release_hook_gate(&gate);
        pohunek_test_support::wait::guard("the hook runner to return after the release", runner)
            .await
            .expect("hook runner joins after the release");
        panic!(
            "run_hook did not terminate the gated hook within the hang guard of {:?}",
            pohunek_test_support::wait::HANG_GUARD
        );
    };
    let warnings = joined.expect("hook runner joins");

    assert_eq!(
        warnings.len(),
        1,
        "one warning for the one hook: {warnings:?}"
    );
    assert_eq!(warnings[0].kind, protocol::SessionWarningKind::Hook);
    assert!(
        warnings[0]
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("was terminated")),
        "the warning names the termination: {warnings:?}"
    );
}

#[tokio::test]
async fn session_stop_hook_reports_stopped_done_and_failed_reasons_once() {
    async fn run_case(tag: &str, command: &str, stop: bool, expected_reason: &str) {
        let config_dir = temp_dir(&format!("session-stop-config-{tag}"));
        let cwd = temp_dir(&format!("session-stop-cwd-{tag}"));
        let store_path = temp_store_path(&format!("session-stop-store-{tag}"));
        let agents_dir = temp_agents_dir_with(
            &format!("session-stop-agent-{tag}"),
            "resumable",
            &format!("base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"{command}\"]\n"),
        );
        let marker = config_dir.join("session-stop.marker");
        write_host_hook(
                &config_dir,
                "session-stop",
                &format!(
                    "#!/bin/sh\nprintf '%s:%s:%s\\n' \"$POHUNEK_HOOK_EVENT\" \"$POHUNEK_SESSION_ID\" \"$POHUNEK_STOP_REASON\" >> {}\n",
                    marker.display()
                ),
            );
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: hermetic_shell(),
            stop_grace: Duration::from_millis(50),
            config_dir: Some(config_dir),
            store_path: Some(store_path.clone()),
            agents_dir: Some(agents_dir),
            ..SessionRegistryConfig::default()
        });

        let created = registry
            .create(SessionNewParams {
                cwd: Some(cwd),
                ..resumable_params()
            })
            .await
            .expect("create session");
        let recorded = registry
            .report_native_id(native_report!(&registry;
                session_id: created.id.clone(),
                agent: "claude".to_owned(),
                native_session_id: format!("native-{tag}"),
                transcript_path: None,
            ))
            .await;
        assert!(recorded.recorded, "native id captured for {tag}");
        assert_eq!(
            crate::store::Store::new(store_path.clone())
                .load_resume()
                .expect("load before terminal")
                .len(),
            1,
            "terminal transition precondition: one resume binding for {tag}"
        );

        if stop {
            registry.stop(&created.id).await.expect("stop session");
        } else {
            registry
                .wait_for_exit(&created.id, HANG_GUARD)
                .await
                .expect("session exits");
        }

        let expected = format!("session-stop:{}:{expected_reason}", created.id.0);
        let contents = wait_for_file_contains(&marker, &expected).await;
        assert_eq!(
            contents.lines().count(),
            1,
            "session-stop fires once for {tag}: {contents:?}"
        );
        assert!(
            crate::store::Store::new(store_path)
                .load_resume()
                .expect("load after terminal")
                .is_empty(),
            "terminal transition must remove resume binding for {tag}"
        );
    }

    run_case("stopped", "sleep 30", true, "stopped").await;
    run_case("done", "sleep 0.2; exit 0", false, "done").await;
    run_case("failed", "sleep 0.2; exit 7", false, "failed").await;
}

#[tokio::test]
async fn agent_state_hook_fires_once_per_distinct_activity_value() {
    let config_dir = temp_dir("agent-state-config");
    let cwd = temp_dir("agent-state-cwd");
    let marker = config_dir.join("agent-state.marker");
    write_host_hook(
            &config_dir,
            "agent-state",
            &format!(
                "#!/bin/sh\nprintf '%s:%s:%s\\n' \"$POHUNEK_HOOK_EVENT\" \"$POHUNEK_SESSION_ID\" \"$POHUNEK_ACTIVITY\" >> {}\n",
                marker.display()
            ),
        );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        config_dir: Some(config_dir),
        ..SessionRegistryConfig::default()
    });
    registry.spawn_agent_state_hooks();
    let created = registry
        .create(SessionNewParams {
            cwd: Some(cwd),
            ..params()
        })
        .await
        .expect("create session");

    registry
        .record_activity(&created.id, transition(AgentActivity::Working))
        .await;
    let mut contents = wait_for_line_count(&marker, 1).await;
    assert!(contents.contains(&format!("agent-state:{}:working", created.id.0)));

    registry
        .record_activity(&created.id, transition(AgentActivity::Working))
        .await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    contents = fs::read_to_string(&marker).expect("read marker");
    assert_eq!(
        contents.lines().count(),
        1,
        "same-state refresh must not fire another hook: {contents:?}"
    );

    for (activity, expected_count) in [
        (AgentActivity::Blocked, 2),
        (AgentActivity::Working, 3),
        (AgentActivity::Idle, 4),
        (AgentActivity::Working, 5),
    ] {
        registry
            .record_activity(&created.id, transition(activity))
            .await;
        contents = wait_for_line_count(&marker, expected_count).await;
    }

    let lines: Vec<String> = contents.lines().map(str::to_owned).collect();
    assert_eq!(lines.len(), 5, "only distinct values fire: {contents:?}");
    assert_eq!(
        lines,
        vec![
            format!("agent-state:{}:working", created.id.0),
            format!("agent-state:{}:blocked", created.id.0),
            format!("agent-state:{}:working", created.id.0),
            format!("agent-state:{}:idle", created.id.0),
            format!("agent-state:{}:working", created.id.0),
        ]
    );

    registry.stop(&created.id).await.expect("stop session");
    registry.shutdown_agent_state_hooks().await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "tracked for session module decomposition"
)]
async fn session_layer_hooks_run_with_cleared_env_and_exact_allowlist() {
    let config_dir = temp_dir("session-hook-env-config");
    let cwd = temp_dir("session-hook-env-cwd");
    let start_env = config_dir.join("session-start.env");
    let state_env = config_dir.join("agent-state.env");
    write_host_hook(
        &config_dir,
        "session-start",
        &format!("#!/bin/sh\nenv > {}\n", start_env.display()),
    );
    write_host_hook(
        &config_dir,
        "agent-state",
        &format!("#!/bin/sh\nenv > {}\n", state_env.display()),
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        config_dir: Some(config_dir),
        ..SessionRegistryConfig::default()
    });
    registry.spawn_agent_state_hooks();
    let created = registry
        .create(SessionNewParams {
            cwd: Some(cwd),
            ..params()
        })
        .await
        .expect("create session");

    wait_for_file_contains(&start_env, "POHUNEK_HOOK_EVENT=session-start").await;
    registry
        .record_activity(&created.id, transition(AgentActivity::Working))
        .await;
    wait_for_file_contains(&state_env, "POHUNEK_ACTIVITY=working").await;

    let start = parse_env_dump(&fs::read_to_string(&start_env).expect("read start env"));
    assert_eq!(
        start.get("POHUNEK_HOOK_EVENT").map(String::as_str),
        Some("session-start")
    );
    assert_eq!(
        start.get("POHUNEK_SESSION_ID").map(String::as_str),
        Some(created.id.0.as_str())
    );
    assert_eq!(
        start.get("POHUNEK_AGENT").map(String::as_str),
        Some("shell")
    );
    assert_eq!(
        pohunek_env_keys(&start),
        [
            "POHUNEK_AGENT",
            "POHUNEK_HOOK_EVENT",
            "POHUNEK_PROJECT_ID",
            "POHUNEK_SESSION_ID",
        ]
        .map(str::to_owned)
        .to_vec()
    );

    let state = parse_env_dump(&fs::read_to_string(&state_env).expect("read state env"));
    assert_eq!(
        state.get("POHUNEK_HOOK_EVENT").map(String::as_str),
        Some("agent-state")
    );
    assert_eq!(
        state.get("POHUNEK_ACTIVITY").map(String::as_str),
        Some("working")
    );
    assert_eq!(
        pohunek_env_keys(&state),
        [
            "POHUNEK_ACTIVITY",
            "POHUNEK_AGENT",
            "POHUNEK_HOOK_EVENT",
            "POHUNEK_PROJECT_ID",
            "POHUNEK_SESSION_ID",
        ]
        .map(str::to_owned)
        .to_vec()
    );

    for env in [&start, &state] {
        assert!(env.contains_key("PATH"), "PATH is passed through");
        assert!(
            !env.keys().any(|key| key.starts_with("CARGO")),
            "daemon inherited CARGO_* env must be cleared: {:?}",
            env.keys().collect::<Vec<_>>()
        );
        for forbidden in [
            "GITHUB_TOKEN",
            "ANTHROPIC_API_KEY",
            "POHUNEK_SOCKET_PATH",
            "POHUNEK_DAEMON_ID",
            "POHUNEK_ENV",
            "POHUNEK_PROTOCOL_VERSION",
            "POHUNEK_REPO",
            "POHUNEK_WORKTREE",
            "POHUNEK_BRANCH",
            "POHUNEK_BASE_BRANCH",
        ] {
            assert!(
                !env.contains_key(forbidden),
                "{forbidden} must not be exposed to a session-layer hook"
            );
        }
    }

    registry.stop(&created.id).await.expect("stop session");
    registry.shutdown_agent_state_hooks().await;
}

#[tokio::test]
async fn in_place_session_fires_session_hooks_but_no_worktree_hooks() {
    let config_dir = temp_dir("in-place-hooks-config");
    let repo = init_git_repo("in-place-hooks-repo");
    let marker = config_dir.join("hooks.marker");
    for event_name in [
        "pre-create",
        "post-create",
        "pre-remove",
        "post-remove",
        "session-start",
        "session-stop",
        "agent-state",
    ] {
        write_host_hook(
            &config_dir,
            event_name,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$POHUNEK_HOOK_EVENT\" >> {}\n",
                marker.display()
            ),
        );
    }
    let store = temp_store_path("in-place-hooks-store");
    let worktree_root = store.parent().expect("store parent").join("worktrees");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store),
        worktree_root: Some(worktree_root),
        config_dir: Some(config_dir),
        ..SessionRegistryConfig::default()
    });
    registry.spawn_agent_state_hooks();

    let created = registry
        .create(SessionNewParams {
            cwd: Some(repo),
            ..params()
        })
        .await
        .expect("create in-place session");
    assert_eq!(created.worktree_path, None, "no --branch means in-place");
    registry
        .record_activity(&created.id, transition(AgentActivity::Working))
        .await;
    wait_for_file_contains(&marker, "agent-state").await;
    registry.stop(&created.id).await.expect("stop session");
    let contents = wait_for_line_count(&marker, 3).await;

    let lines: Vec<&str> = contents.lines().collect();
    assert!(lines.contains(&"session-start"));
    assert!(lines.contains(&"agent-state"));
    assert!(lines.contains(&"session-stop"));
    for forbidden in ["pre-create", "post-create", "pre-remove", "post-remove"] {
        assert!(
            !lines.contains(&forbidden),
            "in-place sessions must not run {forbidden}: {contents:?}"
        );
    }

    registry.shutdown_agent_state_hooks().await;
}

#[tokio::test]
async fn project_backed_session_hooks_receive_project_id() {
    let config_dir = temp_dir("project-session-hooks-config");
    let repo = init_git_repo("project-session-hooks-repo");
    let marker = config_dir.join("project-hooks.marker");
    for event_name in ["session-start", "session-stop", "agent-state"] {
        write_host_hook(
                &config_dir,
                event_name,
                &format!(
                    "#!/bin/sh\nprintf '%s:%s\\n' \"$POHUNEK_HOOK_EVENT\" \"$POHUNEK_PROJECT_ID\" >> {}\n",
                    marker.display()
                ),
            );
    }
    let store = temp_store_path("project-session-hooks-store");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store),
        config_dir: Some(config_dir),
        ..SessionRegistryConfig::default()
    });
    registry.spawn_agent_state_hooks();

    let created = registry
        .create(SessionNewParams {
            cwd: Some(repo),
            ..params()
        })
        .await
        .expect("create project-backed in-place session");
    let project_id = created.project_id.clone().expect("project id stamped");
    wait_for_file_contains(&marker, &format!("session-start:{project_id}")).await;

    registry
        .record_activity(&created.id, transition(AgentActivity::Working))
        .await;
    wait_for_file_contains(&marker, &format!("agent-state:{project_id}")).await;

    registry.stop(&created.id).await.expect("stop session");
    let contents = wait_for_file_contains(&marker, &format!("session-stop:{project_id}")).await;
    for event_name in ["session-start", "agent-state", "session-stop"] {
        assert!(
            contents.contains(&format!("{event_name}:{project_id}")),
            "{event_name} must receive project id {project_id}: {contents:?}"
        );
    }

    registry.shutdown_agent_state_hooks().await;
}

#[tokio::test]
async fn agent_state_hook_dispatcher_survives_lag_and_shutdown_cancellation() {
    let config_dir = temp_dir("agent-state-lag-config");
    let cwd = temp_dir("agent-state-lag-cwd");
    let marker = config_dir.join("agent-state-lag.marker");
    write_host_hook(
        &config_dir,
        "agent-state",
        &format!(
            "#!/bin/sh\nsleep 0.1\nprintf '%s\\n' \"$POHUNEK_ACTIVITY\" >> {}\n",
            marker.display()
        ),
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        config_dir: Some(config_dir),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(SessionNewParams {
            cwd: Some(cwd),
            ..params()
        })
        .await
        .expect("create session");

    registry
        .record_activity(&created.id, transition(AgentActivity::Working))
        .await;
    let (tx, rx) = tokio::sync::broadcast::channel(1);
    for n in 0..8 {
        let _ = tx.send(crate::events::event(
            protocol::event::SESSION_UPDATED,
            serde_json::json!({ "n": n }),
        ));
    }
    let shutdown = tokio_util::sync::CancellationToken::new();
    let handle = super::spawn_agent_state_hook_dispatcher(registry.clone(), rx, shutdown.clone());
    wait_for_file_contains(&marker, "working").await;
    for n in 8..16 {
        let _ = tx.send(crate::events::event(
            protocol::event::SESSION_UPDATED,
            serde_json::json!({ "n": n }),
        ));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let contents = fs::read_to_string(&marker).expect("read marker after same-state lag");
    assert_eq!(
        contents.lines().count(),
        1,
        "lag re-read of the already-fired activity must not double-fire: {contents:?}"
    );

    registry
        .record_activity(&created.id, transition(AgentActivity::Blocked))
        .await;
    tx.send(crate::events::event(
        protocol::event::AGENT_STATE,
        serde_json::json!({
            "session_id": created.id.clone(),
            "activity": "blocked",
            "source": "process",
        }),
    ))
    .expect("send blocked event");
    wait_for_file_contains(&marker, "blocked").await;

    shutdown.cancel();
    guard("the dispatcher to join after cancellation", handle)
        .await
        .expect("dispatcher task succeeds");

    registry.stop(&created.id).await.expect("stop session");
}

#[tokio::test]
async fn agent_state_hook_dispatcher_flushes_buffered_event_on_shutdown() {
    let config_dir = temp_dir("agent-state-shutdown-config");
    let cwd = temp_dir("agent-state-shutdown-cwd");
    let marker = config_dir.join("agent-state-shutdown.marker");
    write_host_hook(
        &config_dir,
        "agent-state",
        &format!(
            "#!/bin/sh\nsleep 0.15\nprintf '%s\\n' \"$POHUNEK_ACTIVITY\" >> {}\n",
            marker.display()
        ),
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        config_dir: Some(config_dir),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(SessionNewParams {
            cwd: Some(cwd),
            ..params()
        })
        .await
        .expect("create session");

    registry
        .record_activity(&created.id, transition(AgentActivity::Working))
        .await;
    let (tx, rx) = tokio::sync::broadcast::channel(16);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let handle = super::spawn_agent_state_hook_dispatcher(registry.clone(), rx, shutdown.clone());
    tx.send(crate::events::event(
        protocol::event::AGENT_STATE,
        serde_json::json!({
            "session_id": created.id.clone(),
            "activity": "working",
            "source": "process",
        }),
    ))
    .expect("send buffered event");
    shutdown.cancel();
    handle.await.expect("dispatcher joins after cancellation");

    let contents = fs::read_to_string(&marker)
        .expect("dispatcher must await the hook flushed during shutdown");
    assert!(
        contents.contains("working"),
        "dispatcher must flush and await buffered activity before joining: {contents:?}"
    );
    registry.stop(&created.id).await.expect("stop session");
}

#[tokio::test]
async fn agent_state_hook_coalesces_flaps_while_hook_is_in_flight() {
    let config_dir = temp_dir("agent-state-coalesce-config");
    let cwd = temp_dir("agent-state-coalesce-cwd");
    let marker = config_dir.join("agent-state-coalesce.marker");
    let release = config_dir.join("agent-state-coalesce.release");
    write_host_hook(
            &config_dir,
            "agent-state",
            &format!(
                "#!/bin/sh\nprintf 'start:%s\\n' \"$POHUNEK_ACTIVITY\" >> {}\nif [ \"$POHUNEK_ACTIVITY\" = working ]; then\n  while [ ! -f {} ]; do sleep 0.02; done\nfi\nprintf 'done:%s\\n' \"$POHUNEK_ACTIVITY\" >> {}\n",
                marker.display(),
                release.display(),
                marker.display(),
            ),
        );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        hook_timeout: Duration::from_secs(1),
        config_dir: Some(config_dir),
        ..SessionRegistryConfig::default()
    });
    registry.spawn_agent_state_hooks();
    let created = registry
        .create(SessionNewParams {
            cwd: Some(cwd),
            ..params()
        })
        .await
        .expect("create session");

    registry
        .record_activity(&created.id, transition(AgentActivity::Working))
        .await;
    let contents = wait_for_file_contains(&marker, "start:working").await;
    assert_eq!(
        contents.lines().collect::<Vec<_>>(),
        vec!["start:working"],
        "the first hook must be in flight before the flap sequence starts"
    );
    registry
        .record_activity(&created.id, transition(AgentActivity::Blocked))
        .await;
    registry
        .record_activity(&created.id, transition(AgentActivity::Idle))
        .await;
    registry
        .record_activity(&created.id, transition(AgentActivity::Blocked))
        .await;
    fs::write(&release, "").expect("release first hook");

    let contents = wait_for_file_contains(&marker, "done:blocked").await;
    assert_eq!(
        contents.lines().collect::<Vec<_>>(),
        vec![
            "start:working",
            "done:working",
            "start:blocked",
            "done:blocked"
        ],
        "only one hook runs in flight per session and intermediate flap is coalesced"
    );

    registry.stop(&created.id).await.expect("stop session");
    registry.shutdown_agent_state_hooks().await;
}

#[test]
fn invalid_agent_activity_parse_returns_error() {
    let err = super::parse_agent_activity(&serde_json::json!("future-state"))
        .expect_err("unknown activity should remain an explicit parse error");
    assert!(
        err.to_string().contains("future-state"),
        "parse error should name the invalid activity: {err}"
    );
}

#[tokio::test]
async fn stop_marks_running_session_stopped() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });

    let created = registry.create(params()).await.expect("create session");
    let mut sessions = registry.inner.sessions.lock().await;
    let entry = sessions.get_mut(&created.id).expect("live session");
    entry.runtime_watch_cancel.cancel();
    entry.info.subagents.push(running_subagent("child-stop", 7));
    drop(sessions);
    let stopped = registry.stop(&created.id).await.expect("stop session");
    let inspected = registry
        .inspect(&created.id)
        .await
        .expect("inspect session");

    assert!(stopped.stopped);
    assert_eq!(inspected.state, SessionState::Stopped);
    assert_eq!(inspected.subagents[0].lifecycle, SubagentLifecycle::Lost);
    assert!(inspected.subagents[0].revision > SubagentRevision::new(7));
}

#[tokio::test]
async fn remove_evicts_an_already_stopped_session() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });

    let created = registry.create(params()).await.expect("create session");
    registry.stop(&created.id).await.expect("stop session");
    let mut events = registry.subscribe();

    let removed = registry.remove(&created.id).await.expect("remove session");

    assert!(removed.removed);
    // The session was already terminal, so removal did not stop it again.
    assert!(!removed.stopped);
    let event = next_session_removed(&mut events).await;
    assert_eq!(event.id, created.id);
    let err = registry
        .inspect(&created.id)
        .await
        .expect_err("removed session is gone");
    assert_eq!(err.code, "session_not_found");
}

#[tokio::test]
async fn remove_stops_a_live_session_then_evicts() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });

    let created = registry.create(params()).await.expect("create session");

    let removed = registry.remove(&created.id).await.expect("remove session");

    assert!(removed.removed);
    // The session was still live, so removal stopped it first.
    assert!(removed.stopped);
    let err = registry
        .inspect(&created.id)
        .await
        .expect_err("removed session is gone");
    assert_eq!(err.code, "session_not_found");
}

#[tokio::test]
async fn remove_refuses_a_conflicted_runtime_not_proven_to_be_its_own() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });

    let created = registry.create(params()).await.expect("create session");
    registry.stop(&created.id).await.expect("stop session");
    let mut sessions = registry.inner.sessions.lock().await;
    let entry = sessions.get_mut(&created.id).expect("stopped session");
    entry.info.state = SessionState::Running;
    entry.runtime = super::RuntimeHandle::Unavailable(RuntimeState::Conflict);
    let runtime = entry.info.runtime.as_mut().expect("runtime");
    runtime.state = RuntimeState::Conflict;
    runtime.loss_reason = Some(crate::runtime::lifecycle::IDENTITY_MISMATCH.to_owned());
    drop(sessions);

    let error = registry
        .remove(&created.id)
        .await
        .expect_err("a conflict over a foreign job or worker is never retired");

    assert_eq!(error.code, "session_runtime_conflict");
    let kept = registry
        .inspect(&created.id)
        .await
        .expect("the refused session stays listed");
    assert_eq!(kept.runtime.expect("runtime").state, RuntimeState::Conflict);
}

#[tokio::test]
async fn delete_session_logs_removes_the_worker_log_family() {
    let log_dir = temp_dir("remove-worker-logs");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        log_dir: Some(log_dir.clone()),
        ..SessionRegistryConfig::default()
    });
    let session_id = SessionId("s-cleanup".to_owned());
    let files =
        pohunek_logging::config::worker_files(&session_id.0).expect("safe managed session id");
    let mut writer = pohunek_logging::Writer::open(
        &log_dir,
        files,
        pohunek_logging::config::worker_policy().expect("valid application policy"),
    )
    .expect("open worker log family");
    writer.write_all(b"{\"worker\":\"running\"}\n").unwrap();
    drop(writer);

    registry
        .delete_session_logs(&session_id)
        .await
        .expect("delete session logs");

    assert!(
        fs::read_dir(&log_dir)
            .expect("read log directory")
            .next()
            .is_none(),
        "session removal must delete its worker log and lock files"
    );
    fs::remove_dir_all(log_dir).expect("remove test log directory");
}

#[tokio::test]
async fn remove_unknown_session_is_session_not_found() {
    let registry = SessionRegistry::default();

    let err = registry
        .remove(&SessionId("s-missing".to_owned()))
        .await
        .expect_err("unknown session cannot be removed");

    assert_eq!(err.code, "session_not_found");
}

#[tokio::test]
async fn attach_tokens_are_one_shot_and_expired_tokens_are_pruned() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(500),
        attach_token_ttl: Duration::from_millis(1),
        ..SessionRegistryConfig::default()
    });

    let created = registry.create(params()).await.expect("create session");
    let expired = registry
        .attach(&attach_params(&created.id))
        .await
        .expect("attach token");
    tokio::time::sleep(Duration::from_millis(5)).await;
    let fresh = registry
        .attach(&attach_params(&created.id))
        .await
        .expect("fresh attach token");

    {
        let pending = registry.inner.pending_attaches.lock().await;
        assert!(
            !pending.contains_key(&expired.stream_id),
            "expired pending attach token should be pruned"
        );
        assert!(
            pending.contains_key(&fresh.stream_id),
            "fresh pending attach token should remain"
        );
    };

    let redeemed = registry
        .redeem_attach(&fresh.stream_id)
        .await
        .expect("redeem fresh attach token");
    let second_redeem = registry
        .redeem_attach(&fresh.stream_id)
        .await
        .expect_err("stream id is one-shot");
    assert_eq!(second_redeem.code, "attach_not_found");

    registry.finish_attach(&redeemed.stream_id, None).await;
    let stopped = registry.stop(&created.id).await.expect("stop session");
    assert!(stopped.stopped);
}

#[tokio::test]
async fn failed_attach_results_are_bounded_and_consumed_once() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        attach_result_capacity: 2,
        ..SessionRegistryConfig::default()
    });
    for stream_id in ["a-oldest", "a-middle", "a-newest"] {
        registry
            .finish_attach(
                stream_id,
                Some(protocol::ProtocolError::new(
                    protocol::ErrorClass::Runtime,
                    "worker_runtime_fault",
                    format!("failure for {stream_id}"),
                    None,
                )),
            )
            .await;
    }

    assert_eq!(
        registry.inner.recent_attach_failures.lock().await.len(),
        2,
        "failed attach mailbox must never exceed its configured capacity"
    );
    let evicted = registry.detach("a-oldest").await;
    assert!(!evicted.detached);
    assert_eq!(evicted.error, None, "oldest result must be evicted first");

    let first = registry.detach("a-middle").await;
    assert!(!first.detached);
    assert_eq!(
        first.error.as_ref().map(|error| error.code.as_str()),
        Some("worker_runtime_fault")
    );
    let consumed = registry.detach("a-middle").await;
    assert_eq!(
        consumed.error, None,
        "a failed attach outcome is returned at most once"
    );
}

#[tokio::test]
async fn failed_attach_results_expire_before_lookup() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        attach_result_ttl: Duration::from_millis(1),
        ..SessionRegistryConfig::default()
    });
    registry
        .finish_attach(
            "a-expired",
            Some(protocol::ProtocolError::new(
                protocol::ErrorClass::Runtime,
                "worker_attach_stream_failed",
                "expired failure",
                None,
            )),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(5)).await;

    let result = registry.detach("a-expired").await;
    assert!(!result.detached);
    assert_eq!(result.error, None);
    assert!(
        registry
            .inner
            .recent_attach_failures
            .lock()
            .await
            .is_empty(),
        "lookup must prune expired attach outcomes"
    );
}

#[tokio::test]
async fn attach_from_inside_the_same_session_is_rejected() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    let daemon_id = registry.daemon_instance_id().to_owned();
    let worker_id = created
        .runtime
        .as_ref()
        .and_then(|runtime| runtime.worker_id.clone())
        .expect("created session has a durable worker id");

    let self_feed = |session: &SessionId, worker: Option<&str>| SessionAttachParams {
        session_id: session.clone(),
        initial_dimensions: None,
        origin_session_id: Some(session.clone()),
        origin_daemon_id: Some(daemon_id.clone()),
        origin_worker_id: worker.map(str::to_owned),
    };

    // Origin id AND worker id both match this session's own worker: the
    // client is inside this session's own PTY, so attaching would loop its
    // output into its own input. Reject it.
    let err = registry
        .attach(&self_feed(&created.id, Some(&worker_id)))
        .await
        .expect_err("self-feeding attach must be rejected");
    assert_eq!(err.code, "attach_self_feedback");
    assert_eq!(err.class, protocol::ErrorClass::Daemon);
    assert!(
        err.recover.is_some(),
        "self-feedback error must carry a recovery hint: {err:?}"
    );
    // The rejected attach mints no pending token.
    assert!(
        registry.inner.pending_attaches.lock().await.is_empty(),
        "a rejected self-feeding attach must not leave a pending token"
    );

    // Matching session id and worker id, but a stale/foreign daemon instance
    // id: the daemon instance id is not an ownership identity (e.g. it
    // changes across a daemon restart while the worker id stays stable), so
    // it must not weaken the worker-id-based guard.
    let err = registry
        .attach(&SessionAttachParams {
            session_id: created.id.clone(),
            initial_dimensions: None,
            origin_session_id: Some(created.id.clone()),
            origin_daemon_id: Some("some-other-daemon".to_owned()),
            origin_worker_id: Some(worker_id.clone()),
        })
        .await
        .expect_err("a stale daemon id must not weaken the worker-id-based guard");
    assert_eq!(err.code, "attach_self_feedback");

    // Same session id but a DIFFERENT worker id (a colliding id on another
    // worker, or a stale value from a previous generation): no loop, so it
    // must be allowed, not falsely rejected.
    registry
        .attach(&self_feed(&created.id, Some("some-other-worker")))
        .await
        .expect("matching session id with a different worker id is allowed");
    // Origin id without any worker id cannot be pinned to this session's worker.
    registry
        .attach(&self_feed(&created.id, None))
        .await
        .expect("origin id without a worker id is allowed");
    // A different session's terminal (this daemon) is a legitimate origin.
    registry
        .attach(&SessionAttachParams {
            session_id: created.id.clone(),
            initial_dimensions: None,
            origin_session_id: Some(SessionId("s-other".to_owned())),
            origin_daemon_id: Some(daemon_id.clone()),
            origin_worker_id: Some(worker_id.clone()),
        })
        .await
        .expect("attach from a different session's terminal is allowed");
    // A plain terminal (no origin reported) is allowed.
    registry
        .attach(&attach_params(&created.id))
        .await
        .expect("attach with no origin is allowed");

    registry.stop(&created.id).await.expect("stop session");
}

#[test]
fn daemon_instance_ids_are_distinct_per_registry() {
    // Two registries built in this one process must still get distinct ids
    // (the process-local counter disambiguates same-instant construction), so
    // the self-feeding-attach guard never conflates two daemon instances.
    let a = SessionRegistry::default();
    let b = SessionRegistry::default();
    assert_ne!(
        a.daemon_instance_id(),
        b.daemon_instance_id(),
        "each registry must get a distinct daemon instance id"
    );
    assert!(a.daemon_instance_id().starts_with("d-"));
}

#[tokio::test]
async fn inspect_missing_session_returns_not_found() {
    let registry = SessionRegistry::default();
    let missing = registry
        .inspect_str("s-missing")
        .await
        .expect_err("missing session");

    assert_eq!(missing.code, "session_not_found");
}

#[tokio::test]
async fn superseded_detector_output_cannot_stamp_replacement_runtime() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");
    let expected = RuntimeWatchIdentity::from_info(&created).expect("live detector identity");
    let original_runtime = {
        let mut sessions = registry.inner.sessions.lock().await;
        let entry = sessions.get_mut(&created.id).expect("session entry");
        let original = entry.info.runtime.clone();
        let runtime = entry.info.runtime.as_mut().expect("runtime projection");
        runtime.worker_instance_id = Some("runtime-replacement".to_owned());
        runtime.runtime_generation = RuntimeGeneration::new(runtime.runtime_generation.get() + 1);
        original
    };

    registry
        .record_detector_activity(&created.id, &expected, transition(AgentActivity::Idle))
        .await;

    let mut sessions = registry.inner.sessions.lock().await;
    let entry = sessions.get_mut(&created.id).expect("session entry");
    assert_eq!(entry.info.activity, None);
    assert_eq!(entry.activity_revision, 0);
    assert!(entry.activity_evidence.is_empty());
    entry.info.runtime = original_runtime;
    drop(sessions);
    let _ = registry.stop(&created.id).await;
}

#[test]
fn bracketed_paste_input_frame_keeps_submit_separate() {
    let writes = super::build_input_writes(
        "hello\nworld",
        InputRules::unrestricted(true, Duration::ZERO),
    )
    .expect("unrestricted input");

    assert_eq!(writes.body, b"\x1b[200~hello\nworld\x1b[201~".to_vec());
    assert_eq!(writes.submit_delay, Duration::ZERO);
}

#[test]
fn delayed_submit_input_frame_splits_text_and_submit() {
    let writes = super::build_input_writes(
        "hello Claude",
        InputRules::unrestricted(false, Duration::from_millis(150)),
    )
    .expect("unrestricted input");

    assert_eq!(writes.body, b"hello Claude".to_vec());
    assert_eq!(writes.submit_delay, Duration::from_millis(150));

    let fragments = super::input::worker_input_fragments(writes, Duration::from_millis(25));
    assert_eq!(fragments.len(), 2);
    assert_eq!(fragments[0].bytes.expose(), b"hello Claude");
    assert_eq!(fragments[0].delay_after_ms, 150);
    assert_eq!(fragments[1].bytes.expose(), super::input::SUBMIT);
    assert_eq!(fragments[1].delay_after_ms, 25);
}

#[test]
fn hermes_multiline_input_is_one_bracketed_paste_then_separate_submit() {
    let rules = builtin_input_rules(&RuntimeRef::hermes(), &SessionRegistryConfig::default());
    let writes = super::build_input_writes("first line\nsecond line", rules)
        .expect("Hermes multiline safe text");

    assert_eq!(
        writes.body,
        b"\x1b[200~first line\nsecond line\x1b[201~".to_vec()
    );
    assert_eq!(writes.submit_delay, Duration::from_millis(150));
}

#[test]
fn hermes_input_rejects_terminal_controls_without_mutating_text() {
    let rules = builtin_input_rules(&RuntimeRef::hermes(), &SessionRegistryConfig::default());
    for unsafe_text in [
        "prompt\u{1b}[201~\rsubmit",
        "prompt\0suffix",
        "prompt\rsuffix",
        "prompt\u{7f}suffix",
        "prompt\u{85}suffix",
    ] {
        let error = super::build_input_writes(unsafe_text, rules)
            .expect_err("Hermes terminal control must be rejected");
        assert_eq!(error.code, "session_input_rejected");
        assert!(!error.msg.contains(unsafe_text));
    }

    let safe = super::build_input_writes("first\n\tsecond", rules)
        .expect("Hermes allows intentional multiline LF and tab");
    assert_eq!(safe.body, b"\x1b[200~first\n\tsecond\x1b[201~".to_vec());
}

#[test]
fn hermes_input_enforces_shared_byte_ceiling() {
    let rules = builtin_input_rules(&RuntimeRef::hermes(), &SessionRegistryConfig::default());
    let at_limit = "x".repeat(protocol::MAX_SESSION_INPUT_BYTES);
    super::build_input_writes(&at_limit, rules).expect("Hermes input at the ceiling is accepted");

    let over_limit = "x".repeat(protocol::MAX_SESSION_INPUT_BYTES + 1);
    let error = super::build_input_writes(&over_limit, rules)
        .expect_err("Hermes input above the shared ceiling must be rejected");
    assert_eq!(error.code, "session_input_rejected");
}

#[test]
fn codex_input_control_behavior_is_unchanged() {
    let rules = builtin_input_rules(&RuntimeRef::codex(), &SessionRegistryConfig::default());
    let text = "prompt\u{1b}[201~\rsubmit";
    let writes = super::build_input_writes(text, rules).expect("Codex remains unrestricted");

    assert!(writes
        .body
        .windows(text.len())
        .any(|window| window == text.as_bytes()));
}

#[test]
fn hermes_programmatic_input_fails_closed_while_blocked() {
    let hermes = builtin_input_rules(&RuntimeRef::hermes(), &SessionRegistryConfig::default());
    let error = hermes
        .validate_activity(Some(AgentActivity::Blocked))
        .expect_err("Hermes input must be denied while approval is visible");
    assert_eq!(error.code, "session_input_blocked");
    assert!(!error.msg.contains("terminal"));

    hermes
        .validate_activity(Some(AgentActivity::Idle))
        .expect("Hermes input is allowed after approval clears");
    let codex = builtin_input_rules(&RuntimeRef::codex(), &SessionRegistryConfig::default());
    codex
        .validate_activity(Some(AgentActivity::Blocked))
        .expect("Codex blocked-input behavior remains unchanged");
}

#[tokio::test]
async fn session_input_wait_rejects_invalid_contract_before_delivery() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: observable_input_shell(),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");

    for timeout_ms in [Some(0), Some(protocol::MAX_SESSION_WAIT_MS + 1)] {
        let error = registry
            .input(protocol::SessionInputParams {
                session_id: created.id.clone(),
                text: "must not be delivered".to_owned(),
                wait: Some(protocol::SessionInputWait {
                    until: Some(vec![AgentActivity::Idle]),
                    timeout_ms,
                }),
            })
            .await
            .expect_err("invalid wait contract must be rejected");
        if timeout_ms == Some(0) {
            assert_eq!(error.code, "session_input_invalid_wait");
        } else {
            assert_eq!(error.code, "session_wait_limit_exceeded");
        }
    }

    tokio::time::sleep(Duration::from_millis(50)).await;
    let output = read_session_output_after_rejection(&registry, &created.id).await;
    assert!(
        !output.contains("must not be delivered"),
        "invalid wait must not deliver input; output={output:?}"
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn session_input_wait_preserves_external_read_only_error() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: observable_input_shell(),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create managed session fixture");
    let external_id = external_session_id(91_337);
    let mut external = created.clone();
    external.id = external_id.clone();
    external.pid = 91_337;
    external.external = Some(true);
    registry
        .inner
        .external
        .upsert_if_current(
            ProcessIdentity {
                pid: external.pid,
                start_identity: StartIdentity::new(1),
            },
            external,
            || Ok::<_, std::convert::Infallible>(true),
            |_| {},
        )
        .await
        .expect("infallible external session validation")
        .expect("current external session");

    let error = registry
        .input(protocol::SessionInputParams {
            session_id: external_id,
            text: "must-not-write".to_owned(),
            wait: Some(protocol::SessionInputWait {
                until: Some(vec![AgentActivity::Idle]),
                timeout_ms: Some(100),
            }),
        })
        .await
        .expect_err("external waited input remains read-only");

    assert_eq!(error.code, "session_external_read_only");
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn stale_external_exit_watch_does_not_remove_reused_pid() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: observable_input_shell(),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create managed session fixture");
    let pid = 91_338;
    let mut external = created.clone();
    external.id = external_session_id(pid);
    external.pid = pid;
    external.external = Some(true);
    let old_identity = ProcessIdentity {
        pid,
        start_identity: StartIdentity::new(10),
    };
    let new_identity = ProcessIdentity {
        pid,
        start_identity: StartIdentity::new(11),
    };
    let published = Mutex::new(Vec::new());

    let first = registry
        .inner
        .external
        .upsert_if_current(
            old_identity,
            external.clone(),
            || Ok::<_, std::convert::Infallible>(true),
            |change| {
                published
                    .lock()
                    .expect("published events lock")
                    .push(change.clone());
            },
        )
        .await
        .expect("infallible old identity validation")
        .expect("current old identity");
    assert_eq!(first.watch_identity, Some(old_identity));
    let replacement = registry
        .inner
        .external
        .upsert_if_current(
            new_identity,
            external.clone(),
            || Ok::<_, std::convert::Infallible>(true),
            |change| {
                published
                    .lock()
                    .expect("published events lock")
                    .push(change.clone());
            },
        )
        .await
        .expect("infallible new identity validation")
        .expect("current new identity");
    assert_eq!(replacement.watch_identity, Some(new_identity));

    assert!(
        !registry
            .inner
            .external
            .remove_identity(old_identity, |info| {
                panic!("stale watch published removal for {}", info.id.0)
            })
            .await
    );
    assert_eq!(published.lock().expect("published events lock").len(), 2);
    assert_eq!(
        registry.inner.external.inspect(&external.id).await,
        Some(external)
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn session_input_wait_times_out_with_typed_error() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");

    let error = registry
        .input(protocol::SessionInputParams {
            session_id: created.id.clone(),
            text: "submitted but never settled".to_owned(),
            wait: Some(protocol::SessionInputWait {
                until: Some(vec![AgentActivity::Blocked]),
                timeout_ms: Some(25),
            }),
        })
        .await
        .expect_err("missing target activity must time out");

    assert_eq!(error.code, "session_input_timeout");
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn session_input_wait_rejects_blocked_session_before_delivery() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: observable_input_shell(),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");
    assert!(
        report_input_activity(&registry, &created.id, AgentActivity::Blocked, 1)
            .await
            .recorded
    );

    let error = registry
        .input(protocol::SessionInputParams {
            session_id: created.id.clone(),
            text: "blocked-input-marker".to_owned(),
            wait: Some(protocol::SessionInputWait {
                until: Some(vec![AgentActivity::Idle]),
                timeout_ms: Some(250),
            }),
        })
        .await
        .expect_err("blocked wait input must be rejected");

    assert_eq!(error.code, "session_agent_blocked");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let output = read_session_output_after_rejection(&registry, &created.id).await;
    assert!(
        !output.contains("blocked-input-marker"),
        "output={output:?}"
    );
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn session_input_wait_rejects_delayed_provider_framing_before_delivery() {
    let agents_dir = temp_agents_dir_with(
        "input-wait-delayed-provider",
        "input-wait-codex",
        "base = \"codex\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", 'while IFS= read -r line; do echo got:$line; done']\n",
    );
    std::fs::write(
        agents_dir.join("input-wait-claude.toml"),
        "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", 'while IFS= read -r line; do echo got:$line; done']\n",
    )
    .expect("write Claude profile");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        agents_dir: Some(agents_dir),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });

    for agent in ["input-wait-codex", "input-wait-claude"] {
        let mut create = params();
        create.agent = agent.to_owned();
        let created = registry.create(create).await.expect("create agent session");
        let marker = format!("unsafe-wait-{agent}");

        let error = registry
            .input(protocol::SessionInputParams {
                session_id: created.id.clone(),
                text: marker.clone(),
                wait: Some(protocol::SessionInputWait {
                    until: Some(Vec::new()),
                    timeout_ms: Some(500),
                }),
            })
            .await
            .expect_err("delayed provider wait must fail before delivery");

        assert_eq!(error.code, "session_input_wait_unsupported");
        registry
            .input(protocol::SessionInputParams {
                session_id: created.id.clone(),
                text: "next-input".to_owned(),
                wait: None,
            })
            .await
            .expect("fire-and-forget keeps provider framing");
        let output = wait_for_session_output(&registry, &created.id, &["got:next-input"]).await;
        assert!(!output.contains(&marker), "output={output:?}");
        let _ = registry.stop(&created.id).await;
    }
}

#[tokio::test]
async fn session_input_wait_rejects_blocked_transition_during_gate_for_all_adapters() {
    let agents_dir = temp_agents_dir_with(
        "input-wait-blocked-gate",
        "input-wait-gate-codex",
        "base = \"codex\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", 'while IFS= read -r line; do echo got:$line; done']\n",
    );
    std::fs::write(
        agents_dir.join("input-wait-gate-claude.toml"),
        "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", 'while IFS= read -r line; do echo got:$line; done']\n",
    )
    .expect("write Claude profile");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        agents_dir: Some(agents_dir),
        shell_command: observable_input_shell(),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });

    for agent in ["shell", "input-wait-gate-codex", "input-wait-gate-claude"] {
        let mut create = params();
        create.agent = agent.to_owned();
        let created = registry.create(create).await.expect("create agent session");
        let input_gate = {
            let sessions = registry.inner.sessions.lock().await;
            std::sync::Arc::clone(&sessions.get(&created.id).expect("session entry").input_gate)
        };
        let gate_guard = input_gate.lock_owned().await;
        let marker = format!("blocked-gate-{agent}");
        let request = tokio::spawn({
            let registry = registry.clone();
            let session_id = created.id.clone();
            let marker = marker.clone();
            async move {
                registry
                    .input(protocol::SessionInputParams {
                        session_id,
                        text: marker,
                        wait: Some(protocol::SessionInputWait {
                            until: Some(vec![AgentActivity::Idle]),
                            timeout_ms: Some(500),
                        }),
                    })
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        registry
            .inner
            .sessions
            .lock()
            .await
            .get_mut(&created.id)
            .expect("session entry")
            .info
            .activity = Some(AgentActivity::Blocked);
        drop(gate_guard);

        let error = request
            .await
            .expect("wait request task joins")
            .expect_err("blocked causal boundary must reject input");
        assert_eq!(error.code, "session_agent_blocked", "agent={agent}");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let output = read_session_output_after_rejection(&registry, &created.id).await;
        assert!(
            !output.contains(&marker),
            "agent={agent}; output={output:?}"
        );
        let _ = registry.stop(&created.id).await;
    }
}

#[tokio::test]
async fn session_input_wait_boundary_excludes_activity_before_worker_write_reservation() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");
    assert!(
        report_input_activity(&registry, &created.id, AgentActivity::Working, 1)
            .await
            .recorded
    );
    let worker = {
        let sessions = registry.inner.sessions.lock().await;
        match &sessions.get(&created.id).expect("session entry").runtime {
            RuntimeHandle::Worker(worker) => worker.clone(),
            RuntimeHandle::Unavailable(state) => panic!("unexpected runtime state: {state:?}"),
        }
    };
    let reservation = worker
        .reserve_write(super::input::worker_input_fragments(
            super::build_input_writes(
                "never-sent-reservation",
                InputRules::unrestricted(false, Duration::ZERO),
            )
            .expect("valid blocker plan"),
            Duration::ZERO,
        ))
        .await
        .expect("reserve worker write");
    let request = tokio::spawn({
        let registry = registry.clone();
        let session_id = created.id.clone();
        async move {
            registry
                .input(protocol::SessionInputParams {
                    session_id,
                    text: "reservation-boundary".to_owned(),
                    wait: Some(protocol::SessionInputWait {
                        until: Some(vec![AgentActivity::Idle]),
                        timeout_ms: Some(150),
                    }),
                })
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        report_input_activity(&registry, &created.id, AgentActivity::Idle, 2)
            .await
            .recorded
    );
    drop(reservation);

    let error = request
        .await
        .expect("waited input task joins")
        .expect_err("pre-reservation activity must not confirm delivered input");
    assert_eq!(error.code, "session_input_timeout");
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn session_input_wait_serializes_activity_snapshot_with_send_start() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");
    assert!(
        report_input_activity(&registry, &created.id, AgentActivity::Working, 1)
            .await
            .recorded
    );
    let mut events = registry.subscribe();
    let send_boundary = Arc::new(tokio::sync::Barrier::new(2));
    registry.hold_next_input_send(Arc::clone(&send_boundary));
    let write = tokio::spawn({
        let registry = registry.clone();
        let session_id = created.id.clone();
        async move {
            registry
                .write_waited_input_with_ack_delay(
                    &session_id,
                    "serialized-boundary",
                    tokio::time::Instant::now() + ORDERING_TEST_INPUT_DEADLINE,
                    Duration::ZERO,
                )
                .await
        }
    });
    send_boundary.wait().await;

    let mut report = Box::pin(report_input_activity(
        &registry,
        &created.id,
        AgentActivity::Idle,
        2,
    ));
    tokio::select! {
        biased;
        result = &mut report => panic!("activity crossed the pre-send boundary: {result:?}"),
        () = tokio::task::yield_now() => {}
    }
    registry
        .inner
        .sessions
        .try_lock()
        .expect_err("activity boundary must retain the session lock until send starts");

    send_boundary.wait().await;
    assert!(report.await.recorded);
    let submission = write
        .await
        .expect("write task joins")
        .expect("write starts after the serialized boundary");
    let result = registry
        .await_input_settled(
            &created.id,
            protocol::SessionInputWait {
                until: Some(vec![AgentActivity::Idle]),
                timeout_ms: Some(1_000),
            },
            submission.clone(),
            &mut events,
        )
        .await
        .expect("post-send activity settles the wait");

    assert!(result.activity_revision > Some(submission.after_revision));
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn session_input_wait_timeout_while_worker_reserved_never_sends_late_input() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: observable_input_shell(),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");
    let worker = {
        let sessions = registry.inner.sessions.lock().await;
        match &sessions.get(&created.id).expect("session entry").runtime {
            RuntimeHandle::Worker(worker) => worker.clone(),
            RuntimeHandle::Unavailable(state) => panic!("unexpected runtime state: {state:?}"),
        }
    };
    let reservation = worker
        .reserve_write(super::input::worker_input_fragments(
            super::build_input_writes(
                "never-sent-reservation",
                InputRules::unrestricted(false, Duration::ZERO),
            )
            .expect("valid blocker plan"),
            Duration::ZERO,
        ))
        .await
        .expect("reserve worker write");

    let error = registry
        .input(protocol::SessionInputParams {
            session_id: created.id.clone(),
            text: "must-not-arrive-late".to_owned(),
            wait: Some(protocol::SessionInputWait {
                until: Some(vec![AgentActivity::Idle]),
                timeout_ms: Some(25),
            }),
        })
        .await
        .expect_err("blocked reservation must consume the overall deadline");
    assert_eq!(error.code, "session_input_timeout");
    drop(reservation);
    tokio::time::sleep(Duration::from_millis(50)).await;
    registry
        .input(protocol::SessionInputParams {
            session_id: created.id.clone(),
            text: "next-input".to_owned(),
            wait: None,
        })
        .await
        .expect("next input succeeds after timed out reservation");
    // The shell answers exactly one line, so its answer names whichever
    // input reached it first.
    let output = wait_for_session_output(&registry, &created.id, &["received:"]).await;
    assert!(
        !output.contains("must-not-arrive-late"),
        "output={output:?}"
    );
    assert!(output.contains("received:next-input"), "output={output:?}");
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn session_input_wait_shutdown_while_worker_reserved_never_sends_late_input() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: observable_input_shell(),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");
    let worker = {
        let sessions = registry.inner.sessions.lock().await;
        match &sessions.get(&created.id).expect("session entry").runtime {
            RuntimeHandle::Worker(worker) => worker.clone(),
            RuntimeHandle::Unavailable(state) => panic!("unexpected runtime state: {state:?}"),
        }
    };
    let reservation = worker
        .reserve_write(super::input::worker_input_fragments(
            super::build_input_writes(
                "never-sent-reservation",
                InputRules::unrestricted(false, Duration::ZERO),
            )
            .expect("valid blocker plan"),
            Duration::ZERO,
        ))
        .await
        .expect("reserve worker write");
    let request = tokio::spawn({
        let registry = registry.clone();
        let session_id = created.id.clone();
        async move {
            registry
                .input(protocol::SessionInputParams {
                    session_id,
                    text: "must-not-arrive-after-shutdown".to_owned(),
                    wait: Some(protocol::SessionInputWait {
                        until: Some(vec![AgentActivity::Idle]),
                        timeout_ms: Some(1_000),
                    }),
                })
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    registry.begin_daemon_shutdown();

    let error = request
        .await
        .expect("waited input task joins")
        .expect_err("shutdown must cancel the unsent reservation");
    assert_eq!(error.code, "daemon_shutting_down");
    drop(reservation);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let output = read_session_output_after_rejection(&registry, &created.id).await;
    assert!(
        !output.contains("must-not-arrive-after-shutdown"),
        "output={output:?}"
    );
    let _ = registry.stop(&created.id).await;
}

/// Deadline of the waited write in
/// [`session_input_wait_timeout_after_atomic_plan_does_not_stage_body`], on the
/// paused clock.
const PLAN_DEADLINE: Duration = Duration::from_millis(50);
/// Worker-side delay before the plan ACK, on the paused clock. It lies past
/// [`PLAN_DEADLINE`], so the ACK is withheld until the test advances the clock
/// to it.
const PLAN_ACK_DELAY: Duration = Duration::from_millis(250);

#[tokio::test]
async fn session_input_wait_timeout_after_atomic_plan_does_not_stage_body() {
    let agents_dir = temp_agents_dir_with(
        "input-wait-plan-ack-deadline",
        "input-wait-plan-ack-claude",
        "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", 'while IFS= read -r line; do echo got:$line; done']\n",
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        agents_dir: Some(agents_dir),
        submit_delay_overrides: submit_delay_override("claude", Duration::ZERO),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let mut create = params();
    create.agent = "input-wait-plan-ack-claude".to_owned();
    let created = registry
        .create(create)
        .await
        .expect("create Claude session");

    // From here the clock moves only through `advance`: the deadline and the
    // worker's ACK delay are virtual timers, while the PTY and the worker
    // socket stay real.
    let inhibitor = AutoAdvanceInhibitor::new();
    tokio::time::pause();
    let send_boundary = Arc::new(tokio::sync::Barrier::new(2));
    registry.hold_next_input_send(Arc::clone(&send_boundary));
    let deadline = tokio::time::Instant::now() + PLAN_DEADLINE;
    let write = tokio::spawn({
        let registry = registry.clone();
        let session_id = created.id.clone();
        async move {
            registry
                .write_waited_input_with_ack_delay(
                    &session_id,
                    "atomic-plan-timeout",
                    deadline,
                    PLAN_ACK_DELAY,
                )
                .await
        }
    });
    send_boundary.wait().await;
    send_boundary.wait().await;
    // The write holds the session lock until its send starts, so acquiring it
    // means the atomic plan is committed to the worker.
    drop(registry.inner.sessions.lock().await);

    tokio::time::advance(PLAN_DEADLINE + TIMER_TICK).await;
    let error = write
        .await
        .expect("waited write task joins")
        .expect_err("delayed plan ACK must time out");
    assert_eq!(error.code, "session_input_timeout");
    let gate_held = {
        let sessions = registry.inner.sessions.lock().await;
        let held = sessions[&created.id].input_gate.try_lock().is_err();
        held
    };
    assert!(
        gate_held,
        "input gate must remain held until the late worker ACK is consumed"
    );

    // Releases the ACK when the worker already armed its delay. Otherwise the
    // remaining delay elapses on the real clock after `resume`.
    tokio::time::advance(PLAN_ACK_DELAY + TIMER_TICK).await;
    tokio::time::resume();
    inhibitor.release().await;

    let next_input = tokio::spawn({
        let registry = registry.clone();
        let session_id = created.id.clone();
        async move {
            registry
                .input(protocol::SessionInputParams {
                    session_id,
                    text: "next-input".to_owned(),
                    wait: None,
                })
                .await
        }
    });
    next_input
        .await
        .expect("next input task joins")
        .expect("next input succeeds after the late plan ACK is consumed");
    let output = wait_for_session_output(
        &registry,
        &created.id,
        &["got:atomic-plan-timeout", "got:next-input"],
    )
    .await;
    assert!(!output.contains("got:atomic-plan-timeoutnext-input"));
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn session_input_serializes_waited_transactions() {
    let agents_dir = temp_agents_dir_with(
        "input-wait-serialized",
        "input-wait-serialized-claude",
        "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", 'while IFS= read -r line; do echo got:$line; done']\n",
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        agents_dir: Some(agents_dir),
        submit_delay_overrides: submit_delay_override("claude", Duration::ZERO),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let mut create = params();
    create.agent = "input-wait-serialized-claude".to_owned();
    let created = registry
        .create(create)
        .await
        .expect("create Claude session");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let first = tokio::spawn({
        let registry = registry.clone();
        let session_id = created.id.clone();
        async move {
            registry
                .write_waited_input_with_ack_delay(
                    &session_id,
                    "first-waited",
                    deadline,
                    Duration::from_millis(100),
                )
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let second = tokio::spawn({
        let registry = registry.clone();
        let session_id = created.id.clone();
        async move {
            registry
                .write_waited_input_with_ack_delay(
                    &session_id,
                    "second-waited",
                    deadline,
                    Duration::ZERO,
                )
                .await
        }
    });

    first
        .await
        .expect("first waited task joins")
        .expect("first waited transaction succeeds");
    second
        .await
        .expect("second waited task joins")
        .expect("second waited transaction succeeds");
    let output = wait_for_session_output(
        &registry,
        &created.id,
        &["got:first-waited", "got:second-waited"],
    )
    .await;
    let first_position = output.find("got:first-waited").expect("first output");
    let second_position = output.find("got:second-waited").expect("second output");
    assert!(first_position < second_position, "output={output:?}");
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn session_input_serializes_waited_and_fire_and_forget_transactions() {
    let agents_dir = temp_agents_dir_with(
        "input-wait-fire-serialized",
        "input-wait-fire-serialized-claude",
        "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", 'while IFS= read -r line; do echo got:$line; done']\n",
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        agents_dir: Some(agents_dir),
        submit_delay_overrides: submit_delay_override("claude", Duration::ZERO),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let mut create = params();
    create.agent = "input-wait-fire-serialized-claude".to_owned();
    let created = registry
        .create(create)
        .await
        .expect("create Claude session");
    let first = tokio::spawn({
        let registry = registry.clone();
        let session_id = created.id.clone();
        async move {
            registry
                .write_waited_input_with_ack_delay(
                    &session_id,
                    "first-waited",
                    tokio::time::Instant::now() + Duration::from_secs(1),
                    Duration::from_millis(100),
                )
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let second = tokio::spawn({
        let registry = registry.clone();
        let session_id = created.id.clone();
        async move {
            registry
                .input(protocol::SessionInputParams {
                    session_id,
                    text: "second-fire".to_owned(),
                    wait: None,
                })
                .await
        }
    });

    first
        .await
        .expect("waited task joins")
        .expect("waited transaction succeeds");
    second
        .await
        .expect("fire task joins")
        .expect("fire-and-forget transaction succeeds");
    let output = wait_for_session_output(
        &registry,
        &created.id,
        &["got:first-waited", "got:second-fire"],
    )
    .await;
    let first_position = output.find("got:first-waited").expect("waited output");
    let second_position = output.find("got:second-fire").expect("fire output");
    assert!(first_position < second_position, "output={output:?}");
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn session_input_wait_accepts_activity_between_submit_flush_and_ack() {
    let agents_dir = temp_agents_dir_with(
        "input-wait-submit-ack",
        "input-wait-ack-claude",
        "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30\"]\n",
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        agents_dir: Some(agents_dir),
        submit_delay_overrides: submit_delay_override("claude", Duration::ZERO),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let mut create = params();
    create.agent = "input-wait-ack-claude".to_owned();
    let created = registry
        .create(create)
        .await
        .expect("create Claude session");
    let mut events = registry.subscribe();
    let send_boundary = Arc::new(tokio::sync::Barrier::new(2));
    registry.hold_next_input_send(Arc::clone(&send_boundary));
    let deadline = tokio::time::Instant::now() + ORDERING_TEST_INPUT_DEADLINE;
    let write = tokio::spawn({
        let registry = registry.clone();
        let session_id = created.id.clone();
        async move {
            registry
                .write_waited_input_with_ack_delay(
                    &session_id,
                    "submit-boundary",
                    deadline,
                    Duration::from_millis(500),
                )
                .await
        }
    });
    send_boundary.wait().await;
    send_boundary.wait().await;

    // The write holds the session lock until its send starts, so both
    // reports order after the boundary; the delayed ACK keeps them before it.
    assert!(
        report_input_activity(&registry, &created.id, AgentActivity::Idle, 1)
            .await
            .recorded
    );
    assert!(
        report_input_activity(&registry, &created.id, AgentActivity::Blocked, 2)
            .await
            .recorded
    );
    assert!(
        !write.is_finished(),
        "activity must be recorded before the delayed worker acknowledgement"
    );
    let submission = write
        .await
        .expect("write task joins")
        .expect("submit write completes after delayed acknowledgement");
    let result = registry
        .await_input_settled(
            &created.id,
            protocol::SessionInputWait {
                until: Some(Vec::new()),
                timeout_ms: Some(1_000),
            },
            submission,
            &mut events,
        )
        .await
        .expect("activity after submit flush but before ACK settles the wait");

    assert_eq!(result.activity, Some(AgentActivity::Idle));
    assert_eq!(result.activity_source, Some(StateSource::Report));
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn session_input_wait_timeout_recheck_rejects_evidence_after_deadline() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");
    let snapshot = registry
        .input_activity_snapshot(&created.id, None)
        .await
        .expect("capture causal boundary");
    tokio::time::pause();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(10);
    let submission = InputSubmission {
        runtime: snapshot.runtime,
        activity_epoch: registry.daemon_instance_id().to_owned(),
        after_revision: snapshot.revision,
        deadline,
    };
    tokio::time::advance(Duration::from_millis(11)).await;
    assert!(
        report_input_activity(&registry, &created.id, AgentActivity::Idle, 1)
            .await
            .recorded
    );

    assert!(registry
        .evaluate_input_wait(&created.id, &[AgentActivity::Idle], &submission, None,)
        .await
        .expect("evaluate late evidence")
        .is_none());
    let mut events = registry.subscribe();
    let error = registry
        .await_input_settled(
            &created.id,
            protocol::SessionInputWait {
                until: Some(vec![AgentActivity::Idle]),
                timeout_ms: Some(10),
            },
            submission,
            &mut events,
        )
        .await
        .expect_err("timeout recheck must reject post-deadline evidence");
    assert_eq!(error.code, "session_input_timeout");
    tokio::time::resume();
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn session_input_wait_retains_pre_deadline_evidence_after_late_same_activity() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");
    let snapshot = registry
        .input_activity_snapshot(&created.id, None)
        .await
        .expect("capture causal boundary");
    tokio::time::pause();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(10);
    let expected_revision = protocol::ActivityRevision::new(snapshot.revision.get() + 1);
    let submission = InputSubmission {
        runtime: snapshot.runtime,
        activity_epoch: registry.daemon_instance_id().to_owned(),
        after_revision: snapshot.revision,
        deadline,
    };
    tokio::time::advance(Duration::from_millis(5)).await;
    assert!(
        report_input_activity(&registry, &created.id, AgentActivity::Idle, 1)
            .await
            .recorded
    );
    tokio::time::advance(Duration::from_millis(6)).await;
    assert!(
        report_input_activity(&registry, &created.id, AgentActivity::Idle, 2)
            .await
            .recorded
    );

    let result = registry
        .evaluate_input_wait(&created.id, &[AgentActivity::Idle], &submission, None)
        .await
        .expect("evaluate retained evidence")
        .expect("pre-deadline evidence remains available");
    assert_eq!(result.activity_revision, Some(expected_revision));
    tokio::time::resume();
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn daemon_reconnect_scopes_reset_activity_revision_with_new_epoch() {
    let first = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let second = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let first_session = first.create(params()).await.expect("create first session");
    let second_session = second
        .create(params())
        .await
        .expect("create second session");
    let first_runtime = first
        .inspect(&first_session.id)
        .await
        .expect("inspect first runtime")
        .runtime
        .expect("first runtime");
    let second_runtime = {
        let mut sessions = second.inner.sessions.lock().await;
        let entry = sessions
            .get_mut(&second_session.id)
            .expect("second session entry");
        entry.info.runtime.replace(first_runtime.clone())
    };
    let mut first_events = first.subscribe();
    let mut second_events = second.subscribe();

    assert!(
        report_input_activity(&first, &first_session.id, AgentActivity::Idle, 1)
            .await
            .recorded
    );
    assert!(
        report_input_activity(&second, &second_session.id, AgentActivity::Idle, 1)
            .await
            .recorded
    );
    let first_event = next_agent_state_event(&mut first_events).await;
    let second_event = next_agent_state_event(&mut second_events).await;

    assert_eq!(first_event.runtime, second_event.runtime);
    assert_eq!(first_event.revision, second_event.revision);
    assert_eq!(
        first_event.revision,
        Some(protocol::ActivityRevision::new(1))
    );
    assert_eq!(
        first_event.activity_epoch.as_deref(),
        Some(first.daemon_instance_id())
    );
    assert_eq!(
        second_event.activity_epoch.as_deref(),
        Some(second.daemon_instance_id())
    );
    assert_ne!(first_event.activity_epoch, second_event.activity_epoch);

    let mut sessions = second.inner.sessions.lock().await;
    sessions
        .get_mut(&second_session.id)
        .expect("second session entry")
        .info
        .runtime = second_runtime;
    drop(sessions);
    let _ = first.stop(&first_session.id).await;
    let _ = second.stop(&second_session.id).await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "the hook-to-journal reconnect scenario is intentionally linear so the trust-boundary assertions remain ordered"
)]
async fn codex_hook_journal_survives_daemon_reconciliation() {
    let store_path = temp_store_path("subagent-hook-reconcile");
    let worker_state_root = store_path
        .parent()
        .expect("store parent")
        .join("worker-state");
    let worker_runtime_root = crate::test_support::thread_scoped_dir("pw-hook-");
    let hook_path = pohunek_test_support::manifest_dir()
        .join("src/integration/assets/codex/pohunek-agent-state.sh");
    let payload = serde_json::json!({
        "session_id": "native-parent",
        "turn_id": "turn-child",
        "hook_event_name": "SubagentStart",
        "agent_id": "child-reconciled",
        "agent_type": "reviewer",
        "transcript_path": "/must/not/cross.jsonl",
        "prompt": "must not cross the hook boundary"
    });
    let command = format!(
        "printf '%s' '{}' | sh '{}' subagent-start; sleep 30",
        payload,
        hook_path.display()
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", command.as_str()]),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        worker_runtime_root: Some(worker_runtime_root.clone()),
        worker_state_root: Some(worker_state_root.clone()),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");

    let observed = wait_until("the hook-reported subagent", || async {
        let info = registry
            .inspect(&created.id)
            .await
            .expect("inspect session");
        info.subagents.first().cloned()
    })
    .await;
    assert_eq!(observed.id, "child-reconciled");
    assert_eq!(observed.lifecycle, SubagentLifecycle::Running);

    let (worker, _identity) = live_worker_and_identity(&registry, &created.id).await;
    assert_eq!(
        worker
            .inspect()
            .await
            .expect("inspect worker journal")
            .subagents
            .len(),
        1
    );
    registry.begin_daemon_shutdown();
    worker
        .release_controller()
        .await
        .expect("release previous daemon controller");
    let store = crate::store::Store::new(store_path.clone());
    let mut record = store
        .load_sessions()
        .expect("load logical record")
        .pop()
        .expect("logical record");
    record.info.subagents.clear();
    store
        .record_session(&record)
        .expect("persist pre-observation daemon snapshot");
    let socket_path = worker_runtime_root
        .join(&created.id.0)
        .join(pohunek_paths::WORKER_SOCKET_NAME);
    let reconnected = Worker::connect_discovered(&socket_path, "replacement-daemon")
        .await
        .expect("reconnect worker");
    let snapshot = reconnected
        .inspect()
        .await
        .expect("inspect after reconnect");
    assert_eq!(snapshot.subagents.len(), 1, "worker journal retained child");
    // The apply accepts a snapshot only while the durable record still names the
    // reconnected worker runtime as live. The worker task that is still running
    // commits its own snapshots, so that record can be momentarily stale between
    // this test's durable write and its apply. Waiting for the documented
    // precondition removes the race without weakening what the apply is asked
    // to prove, and a precondition that never holds says so.
    wait_for_current_worker_metadata_record(&registry, &store, &created.id, &snapshot).await;
    assert_eq!(
        registry
            .apply_worker_metadata_snapshot(&created.id, &snapshot)
            .await,
        WorkerMetadataApplyOutcome::Applied,
        "reconnected snapshot applies"
    );
    registry
        .inner
        .sessions
        .lock()
        .await
        .get_mut(&created.id)
        .expect("session entry")
        .runtime = RuntimeHandle::Worker(reconnected);

    let restored = registry
        .inspect(&created.id)
        .await
        .expect("inspect restored");
    let subagent = restored.subagents.first().expect("restored subagent");
    assert_eq!(subagent.id, "child-reconciled");
    assert_eq!(subagent.agent_type.as_deref(), Some("reviewer"));
    assert_eq!(subagent.lifecycle, SubagentLifecycle::Running);
    let serialized = serde_json::to_string(subagent).expect("serialize public subagent");
    assert!(!serialized.contains("transcript"));
    assert!(!serialized.contains("prompt"));

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn a_worker_without_a_journaled_schema_is_reprojected_with_the_runtime_schema() {
    let store_path = temp_store_path("hook-schema-reprojection");
    let worker_state_root = store_path
        .parent()
        .expect("store parent")
        .join("worker-state");
    let worker_runtime_root = crate::test_support::thread_scoped_dir("pw-schema-");
    let hook_path = pohunek_test_support::manifest_dir()
        .join("src/integration/assets/codex/pohunek-agent-state.sh");
    let payload = serde_json::json!({
        "session_id": "native-parent",
        "turn_id": "turn-child",
        "hook_event_name": "SubagentStart",
        "agent_id": "child-reprojected",
        "agent_type": "reviewer",
    });
    let command = format!(
        "printf '%s' '{}' | sh '{}' subagent-start; sleep 30",
        payload,
        hook_path.display()
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", command.as_str()]),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        worker_runtime_root: Some(worker_runtime_root.clone()),
        worker_state_root: Some(worker_state_root.clone()),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    wait_until("the hook-reported subagent", || async {
        registry
            .inspect(&created.id)
            .await
            .expect("inspect session")
            .subagents
            .first()
            .cloned()
    })
    .await;

    let (worker, _identity) = live_worker_and_identity(&registry, &created.id).await;
    let journaled = worker.inspect().await.expect("inspect worker journal");
    assert_eq!(
        journaled.hook_schema.as_deref(),
        Some("identity-subagent-v1"),
        "the worker journals the schema the daemon delivered at initialize"
    );
    registry.begin_daemon_shutdown();
    worker
        .release_controller()
        .await
        .expect("release previous daemon controller");
    let store = crate::store::Store::new(store_path.clone());
    let mut record = store
        .load_sessions()
        .expect("load logical record")
        .pop()
        .expect("logical record");
    record.info.subagents.clear();
    store
        .record_session(&record)
        .expect("persist pre-observation daemon snapshot");
    let socket_path = worker_runtime_root
        .join(&created.id.0)
        .join(pohunek_paths::WORKER_SOCKET_NAME);
    let reconnected = Worker::connect_discovered(&socket_path, "replacement-daemon")
        .await
        .expect("reconnect worker");
    let mut snapshot = reconnected
        .inspect()
        .await
        .expect("inspect after reconnect");
    // A worker built before schema delivery journals no schema.
    snapshot.hook_schema = None;
    wait_for_current_worker_metadata_record(&registry, &store, &created.id, &snapshot).await;
    assert_eq!(
        registry
            .apply_worker_metadata_snapshot(&created.id, &snapshot)
            .await,
        WorkerMetadataApplyOutcome::Applied,
        "the daemon resolves the schema from the session's pinned runtime"
    );
    registry
        .inner
        .sessions
        .lock()
        .await
        .get_mut(&created.id)
        .expect("session entry")
        .runtime = RuntimeHandle::Worker(reconnected);
    let restored = registry
        .inspect(&created.id)
        .await
        .expect("inspect restored");
    assert_eq!(
        restored
            .subagents
            .first()
            .map(|subagent| subagent.id.as_str()),
        Some("child-reprojected")
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn a_worker_connection_lost_during_daemon_shutdown_leaves_the_runtime_live() {
    let store_path = temp_store_path("shutdown-connection-loss");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    let (worker, _identity) = live_worker_and_identity(&registry, &created.id).await;

    registry.begin_daemon_shutdown();
    // The watcher shares this connection, so its next poll fails as a lost
    // control connection.
    worker
        .release_controller()
        .await
        .expect("release the daemon's controller");
    // The watcher polls its worker every `WORKER_CONNECT_RETRY`; several polls
    // give it time to act on the lost connection.
    tokio::time::sleep(super::WORKER_CONNECT_RETRY * 5).await;

    let memory_state = {
        let sessions = registry.inner.sessions.lock().await;
        let entry = sessions.get(&created.id).expect("live session entry");
        entry.info.runtime.as_ref().map(|runtime| runtime.state)
    };
    assert_eq!(
        memory_state,
        Some(RuntimeState::Live),
        "the next daemon reconciles the runtime, so shutdown rewrites nothing"
    );
    let durable = crate::store::Store::new(store_path)
        .load_sessions()
        .expect("load durable sessions")
        .pop()
        .expect("durable record");
    assert_eq!(durable.runtime.state, RuntimeState::Live);

    let _ = registry.stop(&created.id).await;
}

/// Waits until both records name the snapshot's worker runtime as live.
///
/// Applying worker metadata requires the in-memory and the durable record to be
/// current, and a worker task that is still running commits its own snapshots,
/// so either can be momentarily stale right after a test writes one of its own.
/// Naming whichever record stayed stale keeps a discarded snapshot from being
/// the only thing a failure reports.
async fn wait_for_current_worker_metadata_record(
    registry: &SessionRegistry,
    store: &crate::store::Store,
    id: &SessionId,
    snapshot: &pohunek_worker_protocol::InspectSnapshot,
) {
    let worker_id = snapshot.worker_id.to_string();
    let worker_instance_id = snapshot
        .worker_instance_id
        .as_ref()
        .map(ToString::to_string);
    let is_current = |record: &crate::store::SessionRecord| {
        super::reconcile::worker_metadata_record_is_current(
            record,
            &worker_id,
            worker_instance_id.as_deref(),
        )
    };
    let mut memory_current = false;
    let mut durable_current = false;
    for _ in 0..CONCURRENT_TRANSITION_RETRY_LIMIT {
        memory_current = {
            let sessions = registry.inner.sessions.lock().await;
            let entry = sessions.get(id).expect("live session entry");
            is_current(&SessionRegistry::session_record(
                id,
                entry,
                entry.desired_state,
                None,
            ))
        };
        durable_current = store
            .load_sessions()
            .expect("load durable sessions")
            .pop()
            .is_some_and(|record| is_current(&record));
        if memory_current && durable_current {
            return;
        }
        // `worker_metadata_record_is_current` is a plain bool with no retryable
        // signal, so this waits on the committing task's wall-clock progress
        // rather than on a scheduler turn the way `yield_now()` siblings do.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "worker runtime never settled as live (memory: {memory_current}, durable: {durable_current})"
    );
}

fn worker_subagent_snapshot(id: &str) -> pohunek_worker_protocol::SubagentSnapshot {
    pohunek_worker_protocol::SubagentSnapshot {
        id: id.to_owned(),
        parent_id: None,
        provider: "codex".to_owned(),
        agent_type: Some("reviewer".to_owned()),
        phase: pohunek_worker_protocol::SubagentPhase::Running,
        revision: 1,
        started_at_ms: 1,
        updated_at_ms: 1,
        finished_at_ms: None,
    }
}

#[tokio::test]
async fn worker_metadata_cannot_overwrite_a_terminal_durable_record() {
    let store_path = temp_store_path("worker-metadata-terminal-race");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    let (worker, _identity) = live_worker_and_identity(&registry, &created.id).await;
    let mut snapshot = worker.inspect().await.expect("inspect worker");
    snapshot
        .subagents
        .push(worker_subagent_snapshot("late-child"));

    let store = crate::store::Store::new(store_path);
    let mut terminal = store
        .load_sessions()
        .expect("load live record")
        .pop()
        .expect("live record");
    terminal.desired_state = crate::store::DesiredState::Stopped;
    terminal.info.state = SessionState::Stopped;
    terminal.info.runtime.as_mut().expect("runtime").state = RuntimeState::Terminal;
    terminal.runtime.state = RuntimeState::Terminal;
    store
        .record_session(&terminal)
        .expect("persist terminal record");

    assert_eq!(
        registry
            .apply_worker_metadata_snapshot(&created.id, &snapshot)
            .await,
        WorkerMetadataApplyOutcome::Discarded,
        "terminal durable state must reject late worker metadata"
    );
    assert!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect memory")
            .subagents
            .is_empty(),
        "rejected metadata must not mutate memory"
    );
    let durable = store
        .load_sessions()
        .expect("reload terminal record")
        .pop()
        .expect("terminal record");
    assert_eq!(durable.info.state, SessionState::Stopped);
    assert_eq!(
        durable.info.runtime.expect("runtime").state,
        RuntimeState::Terminal
    );
    assert!(durable.info.subagents.is_empty());
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn worker_metadata_cannot_recreate_a_removed_durable_record() {
    let store_path = temp_store_path("worker-metadata-remove-race");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    let (worker, _identity) = live_worker_and_identity(&registry, &created.id).await;
    let mut snapshot = worker.inspect().await.expect("inspect worker");
    snapshot
        .subagents
        .push(worker_subagent_snapshot("removed-child"));
    let store = crate::store::Store::new(store_path);
    assert!(
        store
            .remove_session(&created.id.0)
            .expect("remove durable session")
            .into_value(),
        "test must remove the durable record"
    );

    assert_eq!(
        registry
            .apply_worker_metadata_snapshot(&created.id, &snapshot)
            .await,
        WorkerMetadataApplyOutcome::Discarded,
        "removed durable state must reject late worker metadata"
    );
    assert!(
        store
            .load_sessions()
            .expect("reload removed store")
            .is_empty(),
        "late metadata must not recreate the removed durable record"
    );
    assert!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect memory")
            .subagents
            .is_empty(),
        "rejected metadata must not mutate memory"
    );
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn failed_worker_metadata_persistence_leaves_memory_retryable() {
    let store_path = temp_store_path("worker-metadata-write-failure");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    let (worker, _identity) = live_worker_and_identity(&registry, &created.id).await;
    let mut snapshot = worker.inspect().await.expect("inspect worker");
    snapshot
        .subagents
        .push(worker_subagent_snapshot("retry-child"));
    let mut events = registry.subscribe();
    registry
        .inner
        .store
        .as_ref()
        .expect("registry store")
        .fail_next_write_before_rename();

    assert_eq!(
        registry
            .apply_worker_metadata_snapshot(&created.id, &snapshot)
            .await,
        WorkerMetadataApplyOutcome::Retryable(WorkerMetadataRetryCause::Commit),
        "failed durable commit must keep the fingerprint retryable"
    );
    assert!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect memory")
            .subagents
            .is_empty(),
        "failed persistence must not mutate memory"
    );

    assert_eq!(
        registry
            .apply_worker_metadata_snapshot(&created.id, &snapshot)
            .await,
        WorkerMetadataApplyOutcome::Applied,
        "the unchanged snapshot must apply after storage recovers"
    );
    assert_eq!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect recovered memory")
            .subagents
            .first()
            .map(|subagent| subagent.id.as_str()),
        Some("retry-child")
    );
    let event = guard("the subagent retry event", async {
        loop {
            let event = events.recv().await.expect("worker metadata event");
            if event.event() == protocol::event::SUBAGENT_STATE {
                break event;
            }
        }
    })
    .await;
    assert_eq!(event.event(), protocol::event::SUBAGENT_STATE);
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn transient_identity_validation_failure_retries_the_same_snapshot() {
    let inspector = Arc::new(MockInspector::default());
    let registry_inspector: Arc<dyn ProcessInspector> = Arc::<MockInspector>::clone(&inspector);
    let registry = SessionRegistry::new_with_inspector(
        SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            procwatch_poll: Duration::from_mins(1),
            ..SessionRegistryConfig::default()
        },
        registry_inspector,
    );
    let created = registry.create(params()).await.expect("create session");
    let (worker, _identity) = live_worker_and_identity(&registry, &created.id).await;
    let mut snapshot = worker.inspect().await.expect("inspect worker");
    let root = snapshot.child_process.expect("worker child");
    snapshot.active_identity = Some(pohunek_worker_protocol::ActiveIdentityClaim {
        provider: "codex".to_owned(),
        process: root,
        sequence: 1,
        expires_at: native_report_expiry(),
        reference_kind: None,
        native_reference: None,
    });
    snapshot
        .subagents
        .push(worker_subagent_snapshot("safe-child"));
    inspector.fail_descendants_with(std::io::ErrorKind::Interrupted);

    assert_eq!(
        registry
            .apply_worker_metadata_snapshot(&created.id, &snapshot)
            .await,
        WorkerMetadataApplyOutcome::Retryable(WorkerMetadataRetryCause::IdentityValidation),
        "transient process inspection failure must request a retry"
    );
    let first = registry
        .inspect(&created.id)
        .await
        .expect("inspect first pass");
    assert!(first.active_agent.is_none());
    assert_eq!(first.subagents.len(), 1, "safe subagents may still commit");

    inspector.clear_descendants_error();
    assert_eq!(
        registry
            .apply_worker_metadata_snapshot(&created.id, &snapshot)
            .await,
        WorkerMetadataApplyOutcome::Applied,
        "the same fingerprint must validate after the transient error clears"
    );
    let retried = registry.inspect(&created.id).await.expect("inspect retry");
    assert_eq!(retried.active_agent.as_deref(), Some("codex"));
    assert_eq!(retried.subagents.len(), 1);
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn subagent_only_metadata_skips_identity_process_validation() {
    let inspector = Arc::new(MockInspector::default());
    let registry_inspector: Arc<dyn ProcessInspector> = Arc::<MockInspector>::clone(&inspector);
    let registry = SessionRegistry::new_with_inspector(
        SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            procwatch_poll: Duration::from_mins(1),
            ..SessionRegistryConfig::default()
        },
        registry_inspector,
    );
    let created = registry.create(params()).await.expect("create session");
    let (worker, _identity) = live_worker_and_identity(&registry, &created.id).await;
    let mut snapshot = worker.inspect().await.expect("inspect worker");
    snapshot.launch_identity = None;
    snapshot.active_identity = None;
    snapshot.active_identity_release = None;
    snapshot
        .subagents
        .push(worker_subagent_snapshot("safe-only-child"));
    inspector.fail_descendants_with(std::io::ErrorKind::Interrupted);

    assert_eq!(
        registry
            .apply_worker_metadata_snapshot(&created.id, &snapshot)
            .await,
        WorkerMetadataApplyOutcome::Applied,
        "safe metadata must not depend on process-tree inspection"
    );
    assert_eq!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect safe metadata")
            .subagents
            .first()
            .map(|subagent| subagent.id.as_str()),
        Some("safe-only-child")
    );
    inspector.clear_descendants_error();
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn worker_metadata_retry_preserves_newer_memory_timestamp() {
    let store_path = temp_store_path("worker-metadata-monotonic-timestamp");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    let (worker, _identity) = live_worker_and_identity(&registry, &created.id).await;
    let mut snapshot = worker.inspect().await.expect("inspect worker");
    snapshot
        .subagents
        .push(worker_subagent_snapshot("timestamp-child"));
    assert_eq!(
        registry
            .apply_worker_metadata_snapshot(&created.id, &snapshot)
            .await,
        WorkerMetadataApplyOutcome::Applied,
        "initial worker metadata must commit"
    );

    let newer_timestamp = "2099-01-01T00:00:00Z";
    let mut sessions = registry.inner.sessions.lock().await;
    let entry = sessions.get_mut(&created.id).expect("session entry");
    // The session's live procwatch observes the shell's real cwd and would
    // overwrite the injected one. Stopping it and marking the injected cwd as
    // the newest evidence confines the test to the metadata rebase.
    entry.procwatch_cancel.cancel();
    entry.cwd_observed_at = crate::time::now();
    entry.info.subagents.clear();
    entry.info.cwd = PathBuf::from("/work/concurrent-cwd");
    entry.info.project_id = Some("p-concurrent".to_owned());
    entry.info.is_linked_worktree = Some(true);
    entry.info.repo = Some(PathBuf::from("/work/concurrent-repo"));
    entry.info.branch = Some("concurrent-branch".to_owned());
    entry.info.worktree_path = Some(PathBuf::from("/work/concurrent-cwd"));
    entry.info.updated_at = newer_timestamp.to_owned();
    drop(sessions);

    assert_eq!(
        registry
            .apply_worker_metadata_snapshot(&created.id, &snapshot)
            .await,
        WorkerMetadataApplyOutcome::Applied,
        "unchanged durable metadata must rebase onto the current memory record"
    );
    let retried = registry.inspect(&created.id).await.expect("inspect retry");
    assert_eq!(retried.updated_at, newer_timestamp);
    assert_eq!(retried.cwd, PathBuf::from("/work/concurrent-cwd"));
    assert_eq!(retried.project_id.as_deref(), Some("p-concurrent"));
    assert_eq!(retried.branch.as_deref(), Some("concurrent-branch"));
    assert_eq!(
        retried
            .subagents
            .first()
            .map(|subagent| subagent.id.as_str()),
        Some("timestamp-child")
    );
    let durable = crate::store::Store::new(store_path)
        .load_sessions()
        .expect("reload rebased metadata")
        .pop()
        .expect("rebased session record");
    assert_eq!(durable.info.updated_at, newer_timestamp);
    assert_eq!(durable.info.cwd, PathBuf::from("/work/concurrent-cwd"));
    assert_eq!(durable.info.project_id.as_deref(), Some("p-concurrent"));
    assert_eq!(durable.info.is_linked_worktree, Some(true));
    assert_eq!(
        durable.info.repo,
        Some(PathBuf::from("/work/concurrent-repo"))
    );
    assert_eq!(durable.info.branch.as_deref(), Some("concurrent-branch"));
    assert_eq!(
        durable.info.worktree_path,
        Some(PathBuf::from("/work/concurrent-cwd"))
    );
    let recovery = durable.recovery.expect("durable recovery binding");
    assert_eq!(recovery.cwd, PathBuf::from("/work/concurrent-cwd"));
    assert_eq!(recovery.project_id.as_deref(), Some("p-concurrent"));
    assert_eq!(recovery.is_linked_worktree, Some(true));
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn durable_launch_identity_survives_worker_metadata_rebase() {
    let store_path = temp_store_path("worker-launch-identity-rebase");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        agents_dir: Some(temp_resumable_agents_dir("worker-launch-identity-rebase")),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(SessionNewParams {
            agent: "resumable".to_owned(),
            ..params()
        })
        .await
        .expect("create resumable session");
    let (worker, _identity) = live_worker_and_identity(&registry, &created.id).await;
    let mut snapshot = worker.inspect().await.expect("inspect worker");
    snapshot.launch_identity = Some(pohunek_worker_protocol::ReportedLaunchIdentity {
        provider: "claude".to_owned(),
        process: snapshot.child_process.expect("worker child"),
        reference_kind: "id".to_owned(),
        native_reference: "durable-launch-native".to_owned(),
    });
    assert_eq!(
        registry
            .apply_worker_metadata_snapshot(&created.id, &snapshot)
            .await,
        WorkerMetadataApplyOutcome::Applied,
        "launch identity must commit"
    );

    let durable = crate::store::Store::new(store_path)
        .load_sessions()
        .expect("load launch identity")
        .pop()
        .expect("durable session");
    assert!(
        durable.native_identity_ordering.is_none(),
        "launch identity has no sequenced ordering key"
    );
    let mut rebased = durable.clone();
    rebased.info.native_session_id = None;
    rebased
        .recovery
        .as_mut()
        .expect("recovery binding")
        .native_session_id = None;
    rebased.info.cwd = PathBuf::from("/work/concurrent-launch-cwd");

    preserve_durable_worker_metadata(&durable, &mut rebased);

    assert_eq!(
        rebased.info.native_session_id.as_deref(),
        Some("durable-launch-native")
    );
    assert_eq!(
        rebased
            .recovery
            .as_ref()
            .and_then(|recovery| recovery.native_session_id.as_deref()),
        Some("durable-launch-native")
    );
    assert_eq!(
        rebased.info.cwd,
        PathBuf::from("/work/concurrent-launch-cwd")
    );
    let _ = registry.stop(&created.id).await;
}

#[test]
fn metadata_tracker_recovers_unchanged_fingerprint_after_previous_limit() {
    let fingerprint = (None, None, None, Vec::new());
    let mut tracker = WorkerMetadataTracker::default();
    let now = Instant::now();

    let mut last_retry = None;
    for expected_attempt in 1..=8 {
        assert!(
            tracker.should_apply(&fingerprint),
            "retryable fingerprint must remain pending after attempt {expected_attempt}"
        );
        let retry = tracker.retry(&fingerprint, now);
        assert_eq!(retry.attempts, expected_attempt);
        assert_eq!(retry.should_warn, expected_attempt == 1);
        last_retry = Some(retry);
    }
    assert_eq!(
        last_retry.expect("retry result").delay,
        MAX_WORKER_METADATA_RETRY_DELAY,
        "exponential retry delay must stop at the configured ceiling"
    );
    assert!(
        tracker.should_apply(&fingerprint),
        "the old three-attempt boundary must not complete retryable metadata"
    );
    assert!(
        !tracker.is_complete(&fingerprint),
        "retryable final metadata must block terminalization"
    );
    assert!(
        !tracker
            .retry(
                &fingerprint,
                (now + WORKER_METADATA_RETRY_WARN_INTERVAL)
                    .checked_sub(Duration::from_millis(1))
                    .expect("warning interval exceeds one millisecond"),
            )
            .should_warn,
        "retry warning must remain throttled inside the interval"
    );
    assert!(
        tracker
            .retry(&fingerprint, now + WORKER_METADATA_RETRY_WARN_INTERVAL,)
            .should_warn,
        "retry warning may repeat after the throttle interval"
    );

    tracker.complete(fingerprint.clone());
    assert!(
        !tracker.should_apply(&fingerprint),
        "successful recovery completes the unchanged fingerprint"
    );
    assert!(
        tracker.is_complete(&fingerprint),
        "recovered final metadata permits terminalization"
    );
}

#[test]
fn terminal_identity_retry_finishes_without_completing_commit_failures() {
    let fingerprint = (None, None, None, Vec::new());
    let now = Instant::now();
    let mut identity_tracker = WorkerMetadataTracker::default();

    assert_eq!(
        identity_tracker.record(
            fingerprint.clone(),
            WorkerMetadataApplyOutcome::Retryable(WorkerMetadataRetryCause::IdentityValidation),
            pohunek_worker_protocol::RuntimePhase::Exited,
            now,
        ),
        WorkerMetadataProgress::IdentityDiscarded
    );
    assert!(
        identity_tracker.is_complete(&fingerprint),
        "unverified terminal identity must not block a known process exit"
    );

    let mut commit_tracker = WorkerMetadataTracker::default();
    assert!(matches!(
        commit_tracker.record(
            fingerprint.clone(),
            WorkerMetadataApplyOutcome::Retryable(WorkerMetadataRetryCause::Commit),
            pohunek_worker_protocol::RuntimePhase::Exited,
            now,
        ),
        WorkerMetadataProgress::Retry(_)
    ));
    assert!(
        !commit_tracker.is_complete(&fingerprint),
        "terminalization must still wait for safe metadata persistence"
    );
}

#[tokio::test]
async fn exit_transition_preserves_subagents_committed_ahead_of_memory() {
    let store_path = temp_store_path("worker-metadata-exit-race");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    let (worker, identity) = live_worker_and_identity(&registry, &created.id).await;
    let mut snapshot = worker.inspect().await.expect("inspect worker");
    snapshot
        .subagents
        .push(worker_subagent_snapshot("durable-exit-child"));
    assert_eq!(
        registry
            .apply_worker_metadata_snapshot(&created.id, &snapshot)
            .await,
        WorkerMetadataApplyOutcome::Applied,
        "worker metadata must commit before the simulated race"
    );

    let mut sessions = registry.inner.sessions.lock().await;
    let entry = sessions.get_mut(&created.id).expect("session entry");
    entry.runtime_watch_cancel.cancel();
    entry.info.subagents.clear();
    entry.info.activity = Some(AgentActivity::Working);
    entry.info.updated_at = timestamp_now();
    drop(sessions);

    assert!(
        registry
            .record_exit(
                &created.id,
                RuntimeExit {
                    exit_code: Some(0),
                    success: true,
                },
                false,
                Some(&identity),
                None,
            )
            .await
            .expect("record exited worker"),
        "exit transition must commit"
    );
    let terminal = registry
        .inspect(&created.id)
        .await
        .expect("inspect terminal");
    assert_eq!(terminal.state, SessionState::Done);
    let child = terminal.subagents.first().expect("terminal subagent");
    assert_eq!(child.id, "durable-exit-child");
    assert_eq!(child.lifecycle, SubagentLifecycle::Lost);

    let durable = crate::store::Store::new(store_path)
        .load_sessions()
        .expect("reload terminal session")
        .pop()
        .expect("terminal record");
    let durable_child = durable
        .info
        .subagents
        .first()
        .expect("durable terminal subagent");
    assert_eq!(durable_child.id, "durable-exit-child");
    assert_eq!(durable_child.lifecycle, SubagentLifecycle::Lost);
    stop_test_worker(worker).await;
}

#[tokio::test]
async fn natural_exit_keeps_an_authorized_attach_open_for_ordered_worker_close() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    let (worker, identity) = live_worker_and_identity(&registry, &created.id).await;
    let cancel = tokio_util::sync::CancellationToken::new();
    registry.inner.active_attaches.lock().await.insert(
        "natural-exit-attach".to_owned(),
        super::ActiveAttach {
            session_id: created.id.clone(),
            cancel: cancel.clone(),
        },
    );

    assert!(
        registry
            .record_exit(
                &created.id,
                RuntimeExit {
                    exit_code: Some(0),
                    success: true,
                },
                false,
                Some(&identity),
                None,
            )
            .await
            .expect("record natural exit"),
        "natural exit transition must commit"
    );
    assert!(
        !cancel.is_cancelled(),
        "natural exit must let the worker deliver queued output and close in order"
    );

    registry
        .inner
        .active_attaches
        .lock()
        .await
        .remove("natural-exit-attach");
    cancel.cancel();
    stop_test_worker(worker).await;
}

#[tokio::test]
async fn exit_transition_preserves_launch_identity_committed_ahead_of_memory() {
    let store_path = temp_store_path("worker-launch-identity-exit-race");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        agents_dir: Some(temp_resumable_agents_dir(
            "worker-launch-identity-exit-race",
        )),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(SessionNewParams {
            agent: "resumable".to_owned(),
            ..params()
        })
        .await
        .expect("create resumable session");
    let (worker, identity) = live_worker_and_identity(&registry, &created.id).await;
    let mut snapshot = worker.inspect().await.expect("inspect worker");
    snapshot.launch_identity = Some(pohunek_worker_protocol::ReportedLaunchIdentity {
        provider: "claude".to_owned(),
        process: snapshot.child_process.expect("worker child"),
        reference_kind: "id".to_owned(),
        native_reference: "durable-exit-native".to_owned(),
    });
    assert_eq!(
        registry
            .apply_worker_metadata_snapshot(&created.id, &snapshot)
            .await,
        WorkerMetadataApplyOutcome::Applied,
        "launch identity must commit before the simulated race"
    );

    let mut sessions = registry.inner.sessions.lock().await;
    let entry = sessions.get_mut(&created.id).expect("session entry");
    entry.runtime_watch_cancel.cancel();
    entry.info.native_session_id = None;
    entry.info.native_session_path = None;
    drop(sessions);

    assert!(
        registry
            .record_exit(
                &created.id,
                RuntimeExit {
                    exit_code: Some(0),
                    success: true,
                },
                false,
                Some(&identity),
                None,
            )
            .await
            .expect("record exited worker"),
        "exit transition must commit"
    );
    let terminal = registry
        .inspect(&created.id)
        .await
        .expect("inspect terminal");
    assert_eq!(terminal.state, SessionState::Done);
    assert_eq!(
        terminal.native_session_id.as_deref(),
        Some("durable-exit-native")
    );

    let durable = crate::store::Store::new(store_path)
        .load_sessions()
        .expect("reload terminal session")
        .pop()
        .expect("terminal record");
    assert_eq!(
        durable.info.native_session_id.as_deref(),
        Some("durable-exit-native")
    );
    assert_eq!(
        durable
            .recovery
            .as_ref()
            .and_then(|recovery| recovery.native_session_id.as_deref()),
        Some("durable-exit-native")
    );
    stop_test_worker(worker).await;
}

#[tokio::test]
async fn lost_transition_preserves_worker_metadata_committed_ahead_of_memory() {
    let store_path = temp_store_path("worker-metadata-lost-race");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        agents_dir: Some(temp_resumable_agents_dir("worker-metadata-lost-race")),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(SessionNewParams {
            agent: "resumable".to_owned(),
            ..params()
        })
        .await
        .expect("create resumable session");
    let (worker, identity) = live_worker_and_identity(&registry, &created.id).await;
    let mut snapshot = worker.inspect().await.expect("inspect worker");
    snapshot.launch_identity = Some(pohunek_worker_protocol::ReportedLaunchIdentity {
        provider: "claude".to_owned(),
        process: snapshot.child_process.expect("worker child"),
        reference_kind: "id".to_owned(),
        native_reference: "durable-lost-native".to_owned(),
    });
    snapshot
        .subagents
        .push(worker_subagent_snapshot("durable-lost-child"));
    assert_eq!(
        registry
            .apply_worker_metadata_snapshot(&created.id, &snapshot)
            .await,
        WorkerMetadataApplyOutcome::Applied,
        "worker metadata must commit before the simulated projection race"
    );

    let mut sessions = registry.inner.sessions.lock().await;
    let entry = sessions.get_mut(&created.id).expect("session entry");
    entry.runtime_watch_cancel.cancel();
    entry.info.subagents.clear();
    entry.info.native_session_id = None;
    entry.info.native_session_path = None;
    drop(sessions);

    assert!(matches!(
        registry
            .mark_worker_unavailable(
                &created.id,
                &identity,
                RuntimeState::Lost,
                crate::session::supervision::RUNTIME_LOST,
            )
            .await,
        super::RuntimeTransitionOutcome::Applied(_)
    ));
    let lost = registry.inspect(&created.id).await.expect("inspect lost");
    assert_eq!(
        lost.runtime.expect("lost runtime").state,
        RuntimeState::Lost
    );
    assert_eq!(
        lost.native_session_id.as_deref(),
        Some("durable-lost-native")
    );
    let child = lost.subagents.first().expect("preserved lost subagent");
    assert_eq!(child.id, "durable-lost-child");
    assert_eq!(child.lifecycle, SubagentLifecycle::Lost);

    let durable = crate::store::Store::new(store_path)
        .load_sessions()
        .expect("reload lost session")
        .pop()
        .expect("durable lost record");
    assert_eq!(
        durable.info.native_session_id.as_deref(),
        Some("durable-lost-native")
    );
    assert_eq!(
        durable
            .recovery
            .as_ref()
            .and_then(|recovery| recovery.native_session_id.as_deref()),
        Some("durable-lost-native")
    );
    assert_eq!(durable.info.subagents[0].id, "durable-lost-child");
    assert_eq!(durable.info.subagents[0].lifecycle, SubagentLifecycle::Lost);
    stop_test_worker(worker).await;
}

#[tokio::test]
async fn failed_stop_intent_persistence_restores_memory_desired_state() {
    let store_path = temp_store_path("stop-intent-rollback");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    registry
        .inner
        .store
        .as_ref()
        .expect("registry store")
        .fail_next_write_before_rename();

    let error = registry
        .stop(&created.id)
        .await
        .expect_err("stop intent persistence must fail");
    assert_eq!(error.code, "session_store_failed");
    let sessions = registry.inner.sessions.lock().await;
    let entry = sessions.get(&created.id).expect("session entry");
    assert_eq!(entry.desired_state, crate::store::DesiredState::Running);
    assert!(!entry.stopping);
    assert!(entry.stop_transaction_id.is_none());
    assert_eq!(entry.info.state, SessionState::Running);
    drop(sessions);

    let durable = crate::store::Store::new(store_path)
        .load_sessions()
        .expect("reload session after failed stop")
        .pop()
        .expect("durable session");
    assert_eq!(durable.desired_state, crate::store::DesiredState::Running);
    assert!(
        registry
            .stop(&created.id)
            .await
            .expect("stop session after storage recovers")
            .stopped
    );
}

#[tokio::test]
async fn failed_stop_intent_rollback_does_not_overwrite_replacement_runtime() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    let (_worker, original_runtime) = live_worker_and_identity(&registry, &created.id).await;
    let mut sessions = registry.inner.sessions.lock().await;
    let entry = sessions.get_mut(&created.id).expect("session entry");
    entry.stopping = true;
    entry.stop_transaction_id = Some("original-stop".to_owned());
    entry.desired_state = crate::store::DesiredState::Stopped;
    entry
        .info
        .runtime
        .as_mut()
        .expect("runtime metadata")
        .worker_instance_id = Some("replacement-runtime".to_owned());
    drop(sessions);

    registry
        .rollback_stop_intent(
            &created.id,
            crate::store::DesiredState::Running,
            crate::store::DesiredState::Stopped,
            "original-stop",
            Some(&original_runtime),
        )
        .await;

    let mut sessions = registry.inner.sessions.lock().await;
    let entry = sessions.get_mut(&created.id).expect("session entry");
    assert_eq!(entry.desired_state, crate::store::DesiredState::Stopped);
    assert!(entry.stopping);
    assert_eq!(entry.stop_transaction_id.as_deref(), Some("original-stop"));
    entry.stopping = false;
    entry.stop_transaction_id = None;
    entry.desired_state = crate::store::DesiredState::Running;
    entry
        .info
        .runtime
        .as_mut()
        .expect("runtime metadata")
        .worker_instance_id = Some(original_runtime.worker_instance_id);
    drop(sessions);
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn user_stop_preserves_subagents_committed_ahead_of_memory() {
    let store_path = temp_store_path("worker-metadata-stop-race");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    let (worker, _identity) = live_worker_and_identity(&registry, &created.id).await;
    let mut snapshot = worker.inspect().await.expect("inspect worker");
    snapshot
        .subagents
        .push(worker_subagent_snapshot("durable-stop-child"));
    assert_eq!(
        registry
            .apply_worker_metadata_snapshot(&created.id, &snapshot)
            .await,
        WorkerMetadataApplyOutcome::Applied,
        "worker metadata must commit before the simulated stop race"
    );

    registry
        .inner
        .sessions
        .lock()
        .await
        .get_mut(&created.id)
        .expect("session entry")
        .info
        .subagents
        .clear();

    assert!(
        registry
            .stop(&created.id)
            .await
            .expect("stop session")
            .stopped,
        "user stop must complete"
    );
    let terminal = registry
        .inspect(&created.id)
        .await
        .expect("inspect stopped session");
    let child = terminal.subagents.first().expect("terminal subagent");
    assert_eq!(child.id, "durable-stop-child");
    assert_eq!(child.lifecycle, SubagentLifecycle::Lost);

    let durable = crate::store::Store::new(store_path)
        .load_sessions()
        .expect("reload stopped session")
        .pop()
        .expect("stopped record");
    let durable_child = durable
        .info
        .subagents
        .first()
        .expect("durable terminal subagent");
    assert_eq!(durable_child.id, "durable-stop-child");
    assert_eq!(durable_child.lifecycle, SubagentLifecycle::Lost);
}

#[tokio::test]
async fn session_input_wait_resnapshots_after_broadcast_lag() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");
    let mut events = registry.subscribe();
    let submitted_revision = registry
        .input_activity_snapshot(&created.id, None)
        .await
        .expect("snapshot activity");
    for sequence in 1..=130 {
        assert!(
            report_input_activity(&registry, &created.id, AgentActivity::Working, sequence,)
                .await
                .recorded
        );
    }
    assert!(
        report_input_activity(&registry, &created.id, AgentActivity::Idle, 131)
            .await
            .recorded
    );
    // The deadline starts after the report burst so a slow host cannot place
    // the Idle evidence past it; the burst is setup, not the bounded wait.
    let submission = InputSubmission {
        runtime: submitted_revision.runtime.clone(),
        activity_epoch: registry.daemon_instance_id().to_owned(),
        after_revision: submitted_revision.revision,
        deadline: tokio::time::Instant::now() + Duration::from_millis(250),
    };

    let result = registry
        .await_input_settled(
            &created.id,
            protocol::SessionInputWait {
                until: Some(vec![AgentActivity::Idle]),
                timeout_ms: Some(100),
            },
            submission,
            &mut events,
        )
        .await
        .expect("lagged receiver must settle from a fresh snapshot");

    assert_eq!(result.activity, Some(AgentActivity::Idle));
    assert_eq!(result.activity_source, Some(StateSource::Report));
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn session_input_wait_uses_rapid_target_event_after_latest_state_changes() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");
    let mut events = registry.subscribe();
    let submitted = registry
        .input_activity_snapshot(&created.id, None)
        .await
        .expect("capture submitted revision");

    assert!(
        report_input_activity(&registry, &created.id, AgentActivity::Idle, 1)
            .await
            .recorded
    );
    assert!(
        report_input_activity(&registry, &created.id, AgentActivity::Working, 2)
            .await
            .recorded
    );
    assert_eq!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect latest activity")
            .activity,
        Some(AgentActivity::Working)
    );
    // The deadline starts after the reports so a slow host cannot place the
    // Idle evidence past it.
    let submission = InputSubmission {
        runtime: submitted.runtime.clone(),
        activity_epoch: registry.daemon_instance_id().to_owned(),
        after_revision: submitted.revision,
        deadline: tokio::time::Instant::now() + Duration::from_millis(250),
    };

    let result = registry
        .await_input_settled(
            &created.id,
            protocol::SessionInputWait {
                until: Some(vec![AgentActivity::Idle]),
                timeout_ms: Some(100),
            },
            submission,
            &mut events,
        )
        .await
        .expect("queued target event settles despite a newer non-target snapshot");

    assert_eq!(result.activity, Some(AgentActivity::Idle));
    assert_eq!(result.runtime.as_ref(), Some(&submitted.runtime));
    assert!(result.activity_revision > Some(submitted.revision));
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn session_input_wait_rejects_replaced_runtime() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");
    let mut events = registry.subscribe();
    let submitted = registry
        .input_activity_snapshot(&created.id, None)
        .await
        .expect("capture original runtime");
    let submission = InputSubmission {
        runtime: submitted.runtime,
        activity_epoch: registry.daemon_instance_id().to_owned(),
        after_revision: submitted.revision,
        deadline: tokio::time::Instant::now() + Duration::from_millis(250),
    };
    let replacement = {
        let mut sessions = registry.inner.sessions.lock().await;
        let entry = sessions.get_mut(&created.id).expect("session entry");
        let runtime = entry.info.runtime.as_mut().expect("managed runtime");
        runtime.worker_instance_id = Some("runtime-replacement".to_owned());
        runtime.runtime_generation =
            protocol::RuntimeGeneration::new(runtime.runtime_generation.get() + 1);
        entry.info.clone()
    };
    registry.emit(protocol::event::SESSION_UPDATED, &replacement);

    let error = registry
        .await_input_settled(
            &created.id,
            protocol::SessionInputWait {
                until: Some(vec![AgentActivity::Idle]),
                timeout_ms: Some(100),
            },
            submission,
            &mut events,
        )
        .await
        .expect_err("replacement runtime must not confirm prior input");

    assert_eq!(error.code, "session_runtime_changed");
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn session_input_wait_rejects_runtime_exit() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");
    let mut events = registry.subscribe();
    let submitted = registry
        .input_activity_snapshot(&created.id, None)
        .await
        .expect("capture running runtime");
    let submission = InputSubmission {
        runtime: submitted.runtime,
        activity_epoch: registry.daemon_instance_id().to_owned(),
        after_revision: submitted.revision,
        deadline: tokio::time::Instant::now() + Duration::from_millis(250),
    };
    registry.stop(&created.id).await.expect("stop session");

    let error = registry
        .await_input_settled(
            &created.id,
            protocol::SessionInputWait {
                until: Some(vec![AgentActivity::Idle]),
                timeout_ms: Some(100),
            },
            submission,
            &mut events,
        )
        .await
        .expect_err("exited runtime must not confirm prior input");

    assert_eq!(error.code, "session_not_running");
}

#[tokio::test]
async fn duplicate_agent_report_sequence_is_idempotent_for_input_waits() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");
    let submitted = registry
        .input_activity_snapshot(&created.id, None)
        .await
        .expect("capture initial revision");
    let submission = InputSubmission {
        runtime: submitted.runtime.clone(),
        activity_epoch: registry.daemon_instance_id().to_owned(),
        after_revision: submitted.revision,
        deadline: tokio::time::Instant::now() + Duration::from_millis(250),
    };
    assert!(
        report_input_activity(&registry, &created.id, AgentActivity::Idle, 7)
            .await
            .recorded
    );
    let first = registry
        .input_activity_snapshot(&created.id, Some(&submitted.runtime))
        .await
        .expect("capture first accepted report");

    let duplicate = report_input_activity(&registry, &created.id, AgentActivity::Blocked, 7).await;
    assert!(!duplicate.recorded);
    let after_duplicate = registry
        .input_activity_snapshot(&created.id, Some(&submitted.runtime))
        .await
        .expect("capture duplicate outcome");
    assert_eq!(after_duplicate.revision, first.revision);
    assert_eq!(after_duplicate.activity, Some(AgentActivity::Idle));

    let mut events = registry.subscribe();
    let error = registry
        .await_input_settled(
            &created.id,
            protocol::SessionInputWait {
                until: Some(vec![AgentActivity::Blocked]),
                timeout_ms: Some(25),
            },
            submission,
            &mut events,
        )
        .await
        .expect_err("duplicate sequence must not synthesize blocked evidence");
    assert_eq!(error.code, "session_input_timeout");
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn session_input_wait_is_cancelled_by_daemon_shutdown() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");
    let wait = tokio::spawn({
        let registry = registry.clone();
        let session_id = created.id.clone();
        async move {
            registry
                .input(protocol::SessionInputParams {
                    session_id,
                    text: "wait for shutdown".to_owned(),
                    wait: Some(protocol::SessionInputWait {
                        until: Some(vec![AgentActivity::Blocked]),
                        timeout_ms: Some(1_000),
                    }),
                })
                .await
        }
    });
    wait_for_session_waiters(&registry, &created.id, 1).await;

    registry.begin_daemon_shutdown();
    let error = wait
        .await
        .expect("input wait task joins")
        .expect_err("shutdown must cancel input wait");

    assert_eq!(error.code, "daemon_shutting_down");
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn session_input_wait_obeys_per_session_waiter_limit() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        observation_global_waiters: 2,
        observation_session_waiters: 1,
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");
    let first = tokio::spawn({
        let registry = registry.clone();
        let session_id = created.id.clone();
        async move {
            registry
                .input(protocol::SessionInputParams {
                    session_id,
                    text: "first waiter".to_owned(),
                    wait: Some(protocol::SessionInputWait {
                        until: Some(vec![AgentActivity::Blocked]),
                        timeout_ms: Some(1_000),
                    }),
                })
                .await
        }
    });
    wait_for_session_waiters(&registry, &created.id, 1).await;

    let error = registry
        .input(protocol::SessionInputParams {
            session_id: created.id.clone(),
            text: "second waiter".to_owned(),
            wait: Some(protocol::SessionInputWait {
                until: Some(vec![AgentActivity::Blocked]),
                timeout_ms: Some(1_000),
            }),
        })
        .await
        .expect_err("second waiter for one session must be rejected");

    assert_eq!(error.code, "session_waiter_limit_reached");
    first.abort();
    let _ = first.await;
    wait_for_session_waiters(&registry, &created.id, 0).await;
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn session_input_wait_deduplicates_targets_and_requires_new_event() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");
    let mut events = registry.subscribe();
    let send_boundary = Arc::new(tokio::sync::Barrier::new(2));
    registry.hold_next_input_send(Arc::clone(&send_boundary));

    let input = tokio::spawn({
        let registry = registry.clone();
        let session_id = created.id.clone();
        async move {
            registry
                .input(protocol::SessionInputParams {
                    session_id,
                    text: "\n".to_owned(),
                    wait: Some(protocol::SessionInputWait {
                        until: Some(vec![
                            AgentActivity::Idle,
                            AgentActivity::Blocked,
                            AgentActivity::Idle,
                        ]),
                        timeout_ms: Some(1_000),
                    }),
                })
                .await
        }
    });
    send_boundary.wait().await;
    send_boundary.wait().await;

    // Ordered after the input boundary: the write keeps the session lock
    // until its send starts.
    let report = report_input_activity(&registry, &created.id, AgentActivity::Idle, 1).await;
    assert!(report.recorded);
    let result = input
        .await
        .expect("input task joins")
        .expect("input waits for a new matching state event");
    assert!(result.accepted);
    assert_eq!(result.activity, Some(AgentActivity::Idle));
    assert_eq!(result.activity_source, Some(StateSource::Report));
    while let Ok(event) = tokio::time::timeout(Duration::from_millis(20), events.recv()).await {
        let _ = event;
    }

    let _ = registry.stop(&created.id).await;
}

async fn read_session_output_after_rejection(registry: &SessionRegistry, id: &SessionId) -> String {
    let output = registry
        .screen(id)
        .await
        .expect("inspect terminal after rejected input");
    output.visible_lines.join("\n")
}

/// Polls the session screen until it shows every needle, so positive output
/// assertions do not depend on how quickly the PTY program answers.
async fn wait_for_session_output(
    registry: &SessionRegistry,
    id: &SessionId,
    needles: &[&str],
) -> String {
    wait_until(
        &format!("session output to contain {needles:?}"),
        || async {
            let output = read_session_output_after_rejection(registry, id).await;
            needles
                .iter()
                .all(|needle| output.contains(needle))
                .then_some(output)
        },
    )
    .await
}

fn observable_input_shell() -> ShellCommand {
    ShellCommand::new(
        "/bin/sh",
        [
            "-c",
            "IFS= read -r line; printf 'received:%s\\n' \"$line\"; sleep 30",
        ],
    )
}

async fn report_input_activity(
    registry: &SessionRegistry,
    session_id: &SessionId,
    activity: AgentActivity,
    sequence: u64,
) -> protocol::SessionReportAgentResult {
    registry
        .report_agent(protocol::SessionReportAgentParams {
            session_id: session_id.clone(),
            source: "test:pohunek-input-wait".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(activity),
            seq: Some(protocol::ReportSequence::new(sequence)),
            pid: None,
            agent_session_id: None,
            agent_session_path: None,
        })
        .await
}

async fn wait_for_session_waiters(
    registry: &SessionRegistry,
    session_id: &SessionId,
    expected: usize,
) {
    wait_until(&format!("{expected} session input waiters"), || async {
        let count = registry
            .inner
            .observation_session_waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .copied()
            .unwrap_or_default();
        (count == expected).then_some(())
    })
    .await;
}

#[test]
fn bare_codex_launch_receives_initial_input_as_prompt_arg() {
    let resolved = crate::agent::ProfileRegistry::default()
        .resolve_agent("codex")
        .expect("resolve bare codex");
    let plan = super::plan_initial_input_delivery(
        &resolved,
        pty_command("codex", []),
        Some("# Pohunek Assistant".to_owned()),
    );

    assert_eq!(plan.command.args, vec!["# Pohunek Assistant".to_owned()]);
    assert_eq!(plan.pending_initial_input, None);
}

#[test]
fn bare_claude_launch_receives_initial_input_as_prompt_arg() {
    let resolved = crate::agent::ProfileRegistry::default()
        .resolve_agent("claude")
        .expect("resolve bare claude");
    let plan = super::plan_initial_input_delivery(
        &resolved,
        pty_command("claude", []),
        Some("# Pohunek Assistant".to_owned()),
    );

    assert_eq!(plan.command.args, vec!["# Pohunek Assistant".to_owned()]);
    assert_eq!(plan.pending_initial_input, None);
}

#[test]
fn shell_launch_keeps_initial_input_for_pty_injection() {
    let resolved = crate::agent::ProfileRegistry::default()
        .resolve_agent("shell")
        .expect("resolve shell");
    let plan = super::plan_initial_input_delivery(
        &resolved,
        pty_command("/bin/sh", ["-c", "sleep 30"]),
        Some("hello shell".to_owned()),
    );

    assert_eq!(plan.pending_initial_input.as_deref(), Some("hello shell"));
}

#[test]
fn bare_hermes_keeps_initial_input_for_pty_injection() {
    let resolved = crate::agent::ProfileRegistry::default()
        .resolve_agent("hermes")
        .expect("resolve bare Hermes");
    let plan = super::plan_initial_input_delivery(
        &resolved,
        pty_command("hermes", ["chat"]),
        Some("first line\nsecond line".to_owned()),
    );

    assert_eq!(plan.command.args, vec!["chat"]);
    assert_eq!(
        plan.pending_initial_input.as_deref(),
        Some("first line\nsecond line")
    );
}

#[test]
fn host_profile_launch_keeps_initial_input_for_pty_injection() {
    let agents_dir = temp_dir("profile-initial-prompt-agents");
    fs::write(
        agents_dir.join("wrapped-codex.toml"),
        "base = \"codex\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30\"]\n",
    )
    .expect("write profile");
    let registry = crate::agent::ProfileRegistry::new(Some(agents_dir));
    let resolved = registry
        .resolve_agent("wrapped-codex")
        .expect("resolve profile");

    let plan = super::plan_initial_input_delivery(
        &resolved,
        pty_command("/bin/sh", ["-c", "sleep 30"]),
        Some("# Pohunek Assistant".to_owned()),
    );

    assert_eq!(
        plan.command.args,
        vec!["-c".to_owned(), "sleep 30".to_owned()]
    );
    assert_eq!(
        plan.pending_initial_input.as_deref(),
        Some("# Pohunek Assistant")
    );
}

#[test]
fn hook_env_injected_for_every_agent_kind_with_socket() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        socket_path: Some(PathBuf::from("/run/pohunek/daemon.sock")),
        ..SessionRegistryConfig::default()
    });
    let id = SessionId("s-7".to_owned());

    for agent in [
        RuntimeRef::shell(),
        RuntimeRef::codex(),
        RuntimeRef::claude(),
        RuntimeRef::hermes(),
    ] {
        let env = registry.hook_env(agent, &id);
        let lookup = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        assert_eq!(lookup(ENV_FLAG).as_deref(), Some("1"));
        assert_eq!(
            lookup(ENV_SOCKET_PATH).as_deref(),
            Some("/run/pohunek/daemon.sock")
        );
        assert_eq!(lookup(ENV_SESSION_ID).as_deref(), Some("s-7"));
        assert_eq!(
            lookup(ENV_PROTOCOL_VERSION).as_deref(),
            Some(protocol::PROTOCOL_VERSION.get().to_string().as_str())
        );
    }
}

#[test]
fn hook_env_absent_without_configured_socket() {
    let registry = SessionRegistry::default();
    let id = SessionId("s-1".to_owned());
    assert!(registry.hook_env(RuntimeRef::shell(), &id).is_empty());
    assert!(registry.hook_env(RuntimeRef::claude(), &id).is_empty());
    assert!(registry.hook_env(RuntimeRef::codex(), &id).is_empty());
}

#[test]
fn session_pty_env_marks_session_id_for_every_agent_kind() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        socket_path: Some(PathBuf::from("/run/pohunek/daemon.sock")),
        ..SessionRegistryConfig::default()
    });
    let id = SessionId("s-7".to_owned());

    // Every kind carries the hook handshake plus POHUNEK_DAEMON_ID. The
    // daemon id keeps self-feeding attach detection scoped to this daemon
    // instance, and POHUNEK_SESSION_ID must not be duplicated on top of the
    // hook env that already carries it.
    for agent in [
        RuntimeRef::shell(),
        RuntimeRef::codex(),
        RuntimeRef::claude(),
    ] {
        let env = registry.session_pty_env(agent.clone(), &id);
        let lookup = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        let session_ids: Vec<&str> = env
            .iter()
            .filter(|(k, _)| k == ENV_SESSION_ID)
            .map(|(_, v)| v.as_str())
            .collect();
        // Present exactly once (agents must not get it duplicated on top of
        // the hook env that already carries it).
        assert_eq!(
            session_ids,
            vec!["s-7"],
            "{agent:?} must carry POHUNEK_SESSION_ID exactly once"
        );
        let daemon_ids: Vec<&str> = env
            .iter()
            .filter(|(k, _)| k == ENV_DAEMON_ID)
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(
            daemon_ids,
            vec![registry.daemon_instance_id()],
            "{agent:?} must carry POHUNEK_DAEMON_ID once, equal to this instance's id"
        );
        assert_eq!(
            lookup(ENV_FLAG).as_deref(),
            Some("1"),
            "{agent:?} must carry the hook gate flag"
        );
        assert_eq!(
            lookup(ENV_SOCKET_PATH).as_deref(),
            Some("/run/pohunek/daemon.sock"),
            "{agent:?} must carry the daemon socket path"
        );
        assert_eq!(
            lookup(ENV_PROTOCOL_VERSION).as_deref(),
            Some(protocol::PROTOCOL_VERSION.get().to_string().as_str()),
            "{agent:?} must carry the protocol version"
        );
    }
}

#[tokio::test]
async fn report_agent_on_shell_session_sets_active_agent_without_changing_launch_identity() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");

    let result = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(ReportSequence::new(1)),
            pid: None,
            agent_session_id: Some("codex-native".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(result.recorded);

    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(inspected.agent, "shell");
    assert_eq!(inspected.agent_base, RuntimeRef::shell());
    assert_eq!(inspected.active_agent.as_deref(), Some("codex"));
    assert_eq!(inspected.active_agent_base, Some(RuntimeRef::codex()));
    assert_eq!(
        inspected.active_agent_session_id.as_deref(),
        Some("codex-native")
    );
    assert_eq!(inspected.active_agent_session_path, None);
    assert_eq!(inspected.native_session_id, None);
    assert_eq!(inspected.native_session_path, None);
    assert_eq!(inspected.activity, Some(AgentActivity::Working));
    assert_eq!(inspected.state_source, StateSource::Report);

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn report_agent_reconfigures_detector_and_release_restores_default_config() {
    let agents_dir = temp_agents_dir_with(
        "active-detector-config",
        "nested-codex",
        "base = \"codex\"\n\
             program = \"/bin/sh\"\n\
             args = [\"-c\", \"sleep 30\"]\n\
             manifest = \"nested-active\"\n",
    );
    write_agent_manifest(
        &agents_dir,
        "nested-active",
        r#"
            [[rules]]
            id = "profile-title"
            state = "blocked"
            priority = 1
            region = "osc_title"
            contains = "profile-only-title"
            "#,
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");
    let mut detector_config_rx = {
        let sessions = registry.inner.sessions.lock().await;
        sessions
            .get(&created.id)
            .expect("session entry")
            .detector_config
            .subscribe()
    };

    let default_config = detector_config_rx.borrow().config.clone();
    assert_eq!(title_activity(&default_config, "profile-only-title"), None);

    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:nested-codex".to_owned(),
            agent: "nested-codex".to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(ReportSequence::new(1)),
            pid: None,
            agent_session_id: None,
            agent_session_path: None,
        })
        .await;
    assert!(report.recorded);
    detector_config_rx
        .changed()
        .await
        .expect("active detector config");
    let active_config = detector_config_rx.borrow().config.clone();
    assert_eq!(
        title_activity(&active_config, "profile-only-title"),
        Some(AgentActivity::Blocked)
    );

    let release = registry
        .release_agent(SessionReleaseAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:nested-codex".to_owned(),
            agent: "nested-codex".to_owned(),
            seq: Some(ReportSequence::new(1)),
        })
        .await;
    assert!(release.released);
    detector_config_rx
        .changed()
        .await
        .expect("default detector config");
    let restored_config = detector_config_rx.borrow().config.clone();
    assert_eq!(title_activity(&restored_config, "profile-only-title"), None);

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn detection_rpc_returns_a_bounded_typed_error_for_oversized_previews() {
    const PREVIEW_COUNT: usize = 128;
    const PREVIEW_TEXT_BYTES: usize = 16 * 1024;

    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");
    let previews = (0..PREVIEW_COUNT)
        .map(|index| DetectionRegionPreview {
            kind: DetectionRegionKind::BottomLines,
            region: format!("bottom_lines({})", index + 1),
            text: "x".repeat(PREVIEW_TEXT_BYTES),
        })
        .collect::<Vec<_>>();
    let (preview_tx, mut preview_rx) = tokio::sync::mpsc::channel(1);
    let mut sessions = registry.inner.sessions.lock().await;
    sessions
        .get_mut(&created.id)
        .expect("session entry")
        .detector_preview = preview_tx;
    drop(sessions);
    tokio::spawn(async move {
        let request = preview_rx.recv().await.expect("preview request");
        request.reply.send(Ok(previews)).expect("preview reply");
    });
    let request = Request::new(
        "x".repeat(MAX_REQUEST_ID_BYTES),
        method::SESSION_DETECTION,
        serde_json::to_value(SessionDetectionParams::new(created.id.clone()))
            .expect("detection params serialize"),
    )
    .expect("valid request");
    let state = DaemonState::new(
        HealthInfo::new("test"),
        registry.clone(),
        Arc::new(crate::governance::HostGovernanceService::open_test()),
        crate::test_support::overlay_registry(),
    );

    let line = serde_json::to_string(&request).expect("request serializes");
    let crate::api::Dispatch::Reply(serialized) = dispatch_line(&line, &state, None).await else {
        panic!("session.detection returns a one-shot reply");
    };
    assert!(serialized.len() <= MAX_CONTROL_LINE_BYTES);
    let response: Response = serde_json::from_str(&serialized).expect("response deserializes");
    let error = response
        .into_result()
        .expect_err("oversized detection returns a typed error");
    assert_eq!(error.class, ErrorClass::Runtime);
    assert_eq!(error.code, "session_detection_response_too_large");

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn osc_7_output_updates_cwd_before_next_procwatch_tick() {
    /// Gives the immediate procwatch tick after spawn time to observe launch cwd.
    const INITIAL_PROCWATCH_SETTLE: Duration = Duration::from_millis(100);

    let cwd = temp_dir("osc7-cwd");
    let script = format!(
        "IFS= read -r _; printf '\\033]7;file://{}\\007'; sleep 30",
        cwd.display()
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", script.as_str()]),
        stop_grace: Duration::from_millis(50),
        procwatch_poll: Duration::from_mins(1),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");
    tokio::time::sleep(INITIAL_PROCWATCH_SETTLE).await;
    registry
        .input(protocol::SessionInputParams {
            session_id: created.id.clone(),
            text: "trigger".to_owned(),
            wait: None,
        })
        .await
        .expect("trigger OSC 7 output");

    let updated = wait_for_cwd_source(&registry, &created.id, &cwd, CwdSource::Osc7).await;

    assert_eq!(updated.cwd, cwd);
    assert_eq!(updated.cwd_source, Some(CwdSource::Osc7));

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn release_agent_clears_current_active_agent_but_ignores_stale_sequence() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");

    let newer = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(ReportSequence::new(10)),
            pid: None,
            agent_session_id: Some("codex-newer".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(newer.recorded);

    let stale_release = registry
        .release_agent(SessionReleaseAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            seq: Some(ReportSequence::new(9)),
        })
        .await;
    assert!(!stale_release.released);
    let still_active = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(still_active.active_agent.as_deref(), Some("codex"));
    assert_eq!(
        still_active.active_agent_session_id.as_deref(),
        Some("codex-newer")
    );

    let current_release = registry
        .release_agent(SessionReleaseAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            seq: Some(ReportSequence::new(10)),
        })
        .await;
    assert!(current_release.released);
    let cleared = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(cleared.active_agent, None);
    assert_eq!(cleared.active_agent_base, None);
    assert_eq!(cleared.active_agent_session_id, None);
    assert_eq!(cleared.active_agent_session_path, None);
    assert_eq!(cleared.activity, None);
    assert_eq!(cleared.state_source, StateSource::Process);

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn report_agent_release_with_no_sequence_does_not_clear_newer_sequenced_report() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");

    let result = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(ReportSequence::new(10)),
            pid: None,
            agent_session_id: Some("codex-newer".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(result.recorded);

    let stale_release = registry
        .release_agent(SessionReleaseAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            seq: None,
        })
        .await;
    assert!(!stale_release.released);

    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(inspected.active_agent.as_deref(), Some("codex"));
    assert_eq!(
        inspected.active_agent_session_id.as_deref(),
        Some("codex-newer")
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn report_agent_with_no_sequence_does_not_overwrite_newer_sequenced_report() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");

    let newer = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(ReportSequence::new(10)),
            pid: None,
            agent_session_id: Some("codex-newer".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(newer.recorded);

    let stale_report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Blocked),
            seq: None,
            pid: None,
            agent_session_id: Some("codex-stale".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(!stale_report.recorded);

    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(
        inspected.active_agent_session_id.as_deref(),
        Some("codex-newer")
    );
    assert_eq!(inspected.activity, Some(AgentActivity::Working));
    assert_eq!(inspected.state_source, StateSource::Report);

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn report_agent_release_tombstone_rejects_delayed_lower_sequence() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");

    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Blocked),
            seq: Some(ReportSequence::new(10)),
            pid: None,
            agent_session_id: Some("codex-seq-10".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(report.recorded);

    let release = registry
        .release_agent(SessionReleaseAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            seq: Some(ReportSequence::new(11)),
        })
        .await;
    assert!(release.released);
    let cleared = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(cleared.active_agent, None);
    assert_eq!(cleared.active_agent_base, None);
    assert_eq!(cleared.active_agent_session_id, None);
    assert_eq!(cleared.active_agent_session_path, None);
    assert_eq!(cleared.activity, None);

    registry
        .record_activity(&created.id, transition(AgentActivity::Working))
        .await;
    let detector_updated = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(detector_updated.activity, Some(AgentActivity::Working));
    assert_eq!(detector_updated.state_source, StateSource::Process);

    let delayed_report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Blocked),
            seq: Some(ReportSequence::new(10)),
            pid: None,
            agent_session_id: Some("codex-stale".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(!delayed_report.recorded);
    let still_clear = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(still_clear.active_agent, None);
    assert_eq!(still_clear.active_agent_base, None);
    assert_eq!(still_clear.active_agent_session_id, None);
    assert_eq!(still_clear.active_agent_session_path, None);
    assert_eq!(still_clear.activity, Some(AgentActivity::Working));
    assert_eq!(still_clear.state_source, StateSource::Process);

    let higher_report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Blocked),
            seq: Some(ReportSequence::new(12)),
            pid: None,
            agent_session_id: Some("codex-seq-12".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(higher_report.recorded);
    let active_again = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(active_again.active_agent.as_deref(), Some("codex"));
    assert_eq!(
        active_again.active_agent_session_id.as_deref(),
        Some("codex-seq-12")
    );
    assert_eq!(active_again.activity, Some(AgentActivity::Blocked));
    assert_eq!(active_again.state_source, StateSource::Report);

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn report_agent_activity_blocks_detector_until_release() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");

    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Blocked),
            seq: Some(ReportSequence::new(1)),
            pid: None,
            agent_session_id: None,
            agent_session_path: None,
        })
        .await;
    assert!(report.recorded);

    registry
        .record_activity(&created.id, transition(AgentActivity::Working))
        .await;
    let blocked = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(blocked.activity, Some(AgentActivity::Blocked));
    assert_eq!(blocked.state_source, StateSource::Report);

    let release = registry
        .release_agent(SessionReleaseAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            seq: Some(ReportSequence::new(1)),
        })
        .await;
    assert!(release.released);

    registry
        .record_activity(&created.id, transition(AgentActivity::Working))
        .await;
    let working = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(working.activity, Some(AgentActivity::Working));
    assert_eq!(working.state_source, StateSource::Process);

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn report_agent_without_activity_keeps_detector_activity_enabled() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");

    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: None,
            seq: Some(ReportSequence::new(1)),
            pid: None,
            agent_session_id: Some("codex-native".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(report.recorded);

    registry
        .record_activity(&created.id, transition(AgentActivity::Working))
        .await;
    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(inspected.active_agent.as_deref(), Some("codex"));
    assert_eq!(
        inspected.active_agent_session_id.as_deref(),
        Some("codex-native")
    );
    assert_eq!(inspected.activity, Some(AgentActivity::Working));
    assert_eq!(inspected.state_source, StateSource::Process);

    let release = registry
        .release_agent(SessionReleaseAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            seq: Some(ReportSequence::new(1)),
        })
        .await;
    assert!(release.released);
    let released = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(released.active_agent, None);
    assert_eq!(released.active_agent_session_id, None);
    assert_eq!(released.activity, Some(AgentActivity::Working));
    assert_eq!(released.state_source, StateSource::Process);

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn report_agent_active_metadata_is_cleared_on_terminal_session() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");
    assert_eq!(created.native_session_id, None);
    assert_eq!(created.native_session_path, None);

    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Blocked),
            seq: Some(ReportSequence::new(1)),
            pid: None,
            agent_session_id: Some("codex-native".to_owned()),
            agent_session_path: Some("/work/codex-session.jsonl".to_owned()),
        })
        .await;
    assert!(report.recorded);

    registry.stop(&created.id).await.expect("stop session");

    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(inspected.active_agent, None);
    assert_eq!(inspected.active_agent_base, None);
    assert_eq!(inspected.active_agent_session_id, None);
    assert_eq!(inspected.active_agent_session_path, None);
    assert_eq!(inspected.activity, None);
    assert_eq!(inspected.native_session_id, None);
    assert_eq!(inspected.native_session_path, None);
}

#[tokio::test]
async fn report_agent_returns_false_for_unknown_agent() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");

    let result = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:unknown".to_owned(),
            agent: "not-a-real-agent".to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(ReportSequence::new(1)),
            pid: None,
            agent_session_id: None,
            agent_session_path: None,
        })
        .await;
    assert!(!result.recorded);

    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(inspected.active_agent, None);
    assert_eq!(inspected.activity, None);

    let _ = registry.stop(&created.id).await;
}

fn codex_fact(process_id: Pid, parent_id: Pid) -> ProcessFact {
    ProcessFact {
        pid: process_id,
        pgid: process_id,
        ppid: parent_id,
        start_identity: crate::procwatch::StartIdentity::new(u64::from(process_id)),
        comm: "codex".to_owned(),
        cmdline: vec!["/usr/bin/codex".to_owned()],
    }
}

fn claude_fact(process_id: Pid, parent_id: Pid) -> ProcessFact {
    ProcessFact {
        pid: process_id,
        pgid: process_id,
        ppid: parent_id,
        start_identity: crate::procwatch::StartIdentity::new(u64::from(process_id)),
        comm: "claude".to_owned(),
        cmdline: vec!["/usr/bin/claude".to_owned()],
    }
}

/// PID used by the P3 release-path matrix to model the nested Codex process.
const RELEASE_MATRIX_AGENT_PID: Pid = 100;
/// `SessionStart` sequence used by the P3 release-path matrix.
const RELEASE_MATRIX_REPORT_SEQ: u64 = 1_000;
/// `SessionEnd` sequence used by the P3 release-path matrix.
const RELEASE_MATRIX_RELEASE_SEQ: u64 = RELEASE_MATRIX_REPORT_SEQ + 1;
/// Hook source used by Codex state callbacks.
const CODEX_HOOK_SOURCE: &str = "pohunek:codex";
/// Agent name used by Codex state callbacks.
const CODEX_HOOK_AGENT: &str = "codex";
/// Native session id used by the P3 release-path matrix.
const RELEASE_MATRIX_NATIVE_ID: &str = "codex-native";
/// Hook sequence used by direct-agent root-pid regression tests.
const DIRECT_AGENT_REPORT_SEQ: u64 = 2_000;
/// Number of reconcile ticks used to catch direct-agent active-agent flapping.
const DIRECT_AGENT_RESCAN_COUNT: usize = 3;
/// PID reused across two scans to model OS pid reuse before an exit watch fires.
const PID_REUSE_AGENT_PID: Pid = 225;
/// PID used by the ownership-marker tests to model a nested daemon's agent.
const FOREIGN_AGENT_PID: Pid = 240;
/// Delay separating pid-reuse observations so `first_seen` changes if reset.
const PID_REUSE_RESCAN_DELAY: Duration = Duration::from_millis(5);
/// Bound proving a blocked foreground probe does not hold the session mutex.
const FOREGROUND_LOCK_TEST_TIMEOUT: Duration = Duration::from_secs(1);

async fn mock_procwatch_registry(tag: &str) -> (SessionRegistry, Arc<MockInspector>, SessionInfo) {
    let inspector = Arc::new(MockInspector::default());
    let registry_inspector: Arc<dyn ProcessInspector> = Arc::<MockInspector>::clone(&inspector);
    let registry = SessionRegistry::new_with_inspector(
        SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            procwatch_poll: Duration::from_mins(1),
            active_agent_claim_ttl: Duration::from_millis(50),
            ..SessionRegistryConfig::default()
        },
        registry_inspector,
    );
    let created = registry
        .create(SessionNewParams {
            name: Some(tag.to_owned()),
            ..params()
        })
        .await
        .expect("create shell session");
    (registry, inspector, created)
}

#[tokio::test]
async fn procwatch_retires_root_authority_before_reused_pid_can_drive_state() {
    let (registry, inspector, created) = mock_procwatch_registry("root-reuse").await;
    let safe_cwd = created.cwd.clone();
    let unrelated_cwd = temp_dir("reused-root-cwd");
    let unrelated_agent = codex_fact(PID_REUSE_AGENT_PID, created.pid);
    inspector.set_descendants(created.pid, vec![unrelated_agent]);
    inspector.set_cwd(created.pid, unrelated_cwd.clone());
    inspector.set_cwd(PID_REUSE_AGENT_PID, unrelated_cwd);

    let (rescan, cancel) = {
        let mut sessions = registry.inner.sessions.lock().await;
        let entry = sessions.get_mut(&created.id).expect("live session entry");
        entry.active_agent = Some(ActiveAgentReport {
            source: "test-root-claim".to_owned(),
            agent: "shell".to_owned(),
            seq: Some(1),
            pid: Some(created.pid),
            start_identity: Some(1),
            reported_at: Instant::now(),
            activity_reported: false,
        });
        entry.info.active_agent = Some("shell".to_owned());
        entry.info.active_agent_base = Some(RuntimeRef::shell());
        entry.info.active_agent_pid = Some(created.pid);
        (
            Arc::clone(&entry.procwatch_rescan),
            entry.procwatch_cancel.clone(),
        )
    };
    inspector.set_identity(
        created.pid,
        Some(ProcessIdentity {
            pid: created.pid,
            start_identity: StartIdentity::new(u64::MAX),
        }),
    );
    rescan.notify_one();
    guard(
        "procwatch to retire a reused root generation",
        cancel.cancelled(),
    )
    .await;

    let inspected = registry
        .inspect(&created.id)
        .await
        .expect("inspect session");
    assert!(inspected.active_agent.is_none());
    assert_eq!(
        inspected.cwd, safe_cwd,
        "a reused root PID must not drive descendant or cwd reconciliation"
    );
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn procwatch_retires_a_zombie_root_with_its_exact_generation() {
    let (registry, inspector, created) = mock_procwatch_registry("zombie-root").await;
    let root = inspector
        .identity(created.pid)
        .expect("root identity probe")
        .expect("live root identity");
    let expected = RuntimeWatchIdentity::from_info(&created).expect("runtime identity");
    let () = {
        let mut sessions = registry.inner.sessions.lock().await;
        let entry = sessions.get_mut(&created.id).expect("live session entry");
        entry.active_agent = Some(ActiveAgentReport {
            source: "test-zombie-claim".to_owned(),
            agent: "shell".to_owned(),
            seq: Some(1),
            pid: Some(root.pid),
            start_identity: Some(root.start_identity.get()),
            reported_at: Instant::now(),
            activity_reported: false,
        });
        entry.info.active_agent = Some("shell".to_owned());
        entry.info.active_agent_base = Some(RuntimeRef::shell());
        entry.info.active_agent_pid = Some(root.pid);
    };
    inspector.set_not_running(root);

    assert!(
        !registry
            .rescan_live_root(&created.id, &expected, root, Instant::now())
            .await,
        "a zombie must release procwatch root authority"
    );
    assert!(registry
        .inspect(&created.id)
        .await
        .expect("inspect session")
        .active_agent
        .is_none());
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn procwatch_discards_scan_when_root_generation_changes_inside_bracket() {
    let (registry, inspector, created) = mock_procwatch_registry("root-bracket").await;
    let safe_cwd = created.cwd.clone();
    inspector.set_descendants(
        created.pid,
        vec![codex_fact(PID_REUSE_AGENT_PID, created.pid)],
    );
    inspector.set_cwd(created.pid, temp_dir("root-bracket-reused"));
    inspector.change_identity_after_descendants(
        created.pid,
        ProcessIdentity {
            pid: created.pid,
            start_identity: StartIdentity::new(u64::MAX),
        },
    );

    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;

    let inspected = registry
        .inspect(&created.id)
        .await
        .expect("inspect session");
    assert!(inspected.active_agent.is_none());
    assert_eq!(inspected.cwd, safe_cwd);
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn procwatch_discards_cwd_when_focus_generation_changes_inside_bracket() {
    let (registry, inspector, created) = mock_procwatch_registry("focus-bracket").await;
    let safe_cwd = created.cwd.clone();
    let agent = codex_fact(PID_REUSE_AGENT_PID, created.pid);
    inspector.set_descendants(created.pid, vec![agent.clone()]);
    inspector.set_cwd(PID_REUSE_AGENT_PID, temp_dir("focus-bracket-reused"));
    {
        let mut sessions = registry.inner.sessions.lock().await;
        let entry = sessions.get_mut(&created.id).expect("live session entry");
        entry.active_agent = Some(ActiveAgentReport {
            source: "test-focus-claim".to_owned(),
            agent: "codex".to_owned(),
            seq: Some(1),
            pid: Some(agent.pid),
            start_identity: Some(agent.start_identity.get()),
            reported_at: Instant::now(),
            activity_reported: false,
        });
        entry.info.active_agent = Some("codex".to_owned());
        entry.info.active_agent_base = Some(RuntimeRef::codex());
        entry.info.active_agent_pid = Some(agent.pid);
    };
    inspector.change_identity_after_cwd(
        agent.pid,
        ProcessIdentity {
            pid: agent.pid,
            start_identity: StartIdentity::new(agent.start_identity.get() + 1),
        },
    );

    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;

    assert_eq!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect session")
            .cwd,
        safe_cwd,
        "a reused focus PID cannot apply its cwd"
    );
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn stale_procwatch_retirement_cannot_clear_replacement_runtime() {
    let (registry, _inspector, created) = mock_procwatch_registry("stale-retirement").await;
    let expected = RuntimeWatchIdentity::from_info(&created).expect("runtime identity");
    let () = {
        let mut sessions = registry.inner.sessions.lock().await;
        let entry = sessions.get_mut(&created.id).expect("live session entry");
        let runtime = entry.info.runtime.as_mut().expect("runtime info");
        runtime.worker_instance_id = Some("runtime-replacement".to_owned());
        runtime.runtime_generation = RuntimeGeneration::new(runtime.runtime_generation.get() + 1);
        entry.active_agent = Some(ActiveAgentReport {
            source: "replacement-claim".to_owned(),
            agent: "codex".to_owned(),
            seq: Some(2),
            pid: Some(PID_REUSE_AGENT_PID),
            start_identity: Some(2),
            reported_at: Instant::now(),
            activity_reported: false,
        });
        entry.info.active_agent = Some("codex".to_owned());
        entry.info.active_agent_base = Some(RuntimeRef::codex());
        entry.info.active_agent_pid = Some(PID_REUSE_AGENT_PID);
    };

    registry
        .retire_procwatch_root(&created.id, &expected, Instant::now())
        .await;

    let inspected = registry
        .inspect(&created.id)
        .await
        .expect("inspect replacement");
    assert_eq!(inspected.active_agent.as_deref(), Some("codex"));
    assert_eq!(
        inspected
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.worker_instance_id.as_deref()),
        Some("runtime-replacement")
    );
    let () = {
        let mut sessions = registry.inner.sessions.lock().await;
        let entry = sessions.get_mut(&created.id).expect("replacement entry");
        entry.info.runtime = created.runtime.clone();
    };
    let _ = registry.stop(&created.id).await;
}

async fn mock_direct_codex_registry(
    tag: &str,
) -> (SessionRegistry, Arc<MockInspector>, SessionInfo) {
    let inspector = Arc::new(MockInspector::default());
    let registry_inspector: Arc<dyn ProcessInspector> = Arc::<MockInspector>::clone(&inspector);
    let agents_dir = temp_agents_dir_with(
        tag,
        "direct-codex",
        "base = \"codex\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30\"]\n",
    );
    let registry = SessionRegistry::new_with_inspector(
        SessionRegistryConfig {
            shell_command: hermetic_shell(),
            stop_grace: Duration::from_millis(50),
            procwatch_poll: Duration::from_mins(1),
            active_agent_claim_ttl: Duration::from_millis(50),
            agents_dir: Some(agents_dir),
            ..SessionRegistryConfig::default()
        },
        registry_inspector,
    );
    let created = registry
        .create(SessionNewParams {
            name: Some(tag.to_owned()),
            agent: "direct-codex".to_owned(),
            ..params()
        })
        .await
        .expect("create direct codex session");
    (registry, inspector, created)
}

async fn wait_for_active_agent_pid(
    registry: &SessionRegistry,
    id: &SessionId,
    expected: Option<Pid>,
) -> SessionInfo {
    wait_until(&format!("active_agent_pid {expected:?}"), || async {
        let info = registry.inspect(id).await.expect("inspect session");
        (info.active_agent_pid == expected).then_some(info)
    })
    .await
}

async fn report_release_matrix_agent(registry: &SessionRegistry, created: &SessionInfo) {
    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: CODEX_HOOK_SOURCE.to_owned(),
            agent: CODEX_HOOK_AGENT.to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(ReportSequence::new(RELEASE_MATRIX_REPORT_SEQ)),
            pid: Some(RELEASE_MATRIX_AGENT_PID),
            agent_session_id: Some(RELEASE_MATRIX_NATIVE_ID.to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(report.recorded);

    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(inspected.active_agent.as_deref(), Some(CODEX_HOOK_AGENT));
    assert_eq!(inspected.active_agent_base, Some(RuntimeRef::codex()));
    assert_eq!(inspected.active_agent_pid, Some(RELEASE_MATRIX_AGENT_PID));
    assert_eq!(
        inspected.active_agent_session_id.as_deref(),
        Some(RELEASE_MATRIX_NATIVE_ID)
    );
}

#[tokio::test]
async fn direct_agent_root_pid_hook_claim_survives_procwatch_reconciles() {
    let (registry, inspector, created) = mock_direct_codex_registry("direct-root").await;
    inspector.set_descendants(created.pid, Vec::new());
    inspector.set_cwd(created.pid, temp_dir("direct-root-cwd"));
    inspector.set_foreground_group(created.pid, Some(created.pid));

    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: CODEX_HOOK_SOURCE.to_owned(),
            agent: CODEX_HOOK_AGENT.to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(ReportSequence::new(DIRECT_AGENT_REPORT_SEQ)),
            pid: Some(created.pid),
            agent_session_id: Some("direct-native".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(report.recorded);

    for _ in 0..DIRECT_AGENT_RESCAN_COUNT {
        registry
            .rescan_procwatch_at(&created.id, created.pid, Instant::now())
            .await;
        let inspected = registry.inspect(&created.id).await.expect("inspect");
        assert_eq!(inspected.active_agent.as_deref(), Some(CODEX_HOOK_AGENT));
        assert_eq!(inspected.active_agent_base, Some(RuntimeRef::codex()));
        assert_eq!(inspected.active_agent_pid, Some(created.pid));
        assert_eq!(
            inspected.active_agent_session_id.as_deref(),
            Some("direct-native")
        );
    }

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn direct_agent_without_hook_does_not_auto_report_root() {
    let (registry, inspector, created) = mock_direct_codex_registry("direct-no-hook").await;
    inspector.set_descendants(created.pid, Vec::new());
    inspector.set_cwd(created.pid, temp_dir("direct-no-hook-cwd"));
    inspector.set_foreground_group(created.pid, Some(created.pid));

    for _ in 0..DIRECT_AGENT_RESCAN_COUNT {
        registry
            .rescan_procwatch_at(&created.id, created.pid, Instant::now())
            .await;
        let inspected = registry.inspect(&created.id).await.expect("inspect");
        assert_eq!(inspected.active_agent, None);
        assert_eq!(inspected.active_agent_base, None);
        assert_eq!(inspected.active_agent_pid, None);
    }

    let _ = registry.stop(&created.id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_probe_does_not_hold_global_session_lock() {
    let (registry, inspector, created) = mock_procwatch_registry("foreground-lock").await;
    inspector.set_descendants(created.pid, Vec::new());
    inspector.set_foreground_group(created.pid, Some(created.pid));
    let block = inspector.block_foreground();
    let scan_registry = registry.clone();
    let scan_id = created.id.clone();
    let root_pid = created.pid;
    let scan = tokio::spawn(async move {
        scan_registry
            .rescan_procwatch_at(&scan_id, root_pid, Instant::now())
            .await;
    });

    tokio::time::timeout(FOREGROUND_LOCK_TEST_TIMEOUT, async {
        while !block.entered.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("foreground probe should start");

    tokio::time::timeout(FOREGROUND_LOCK_TEST_TIMEOUT, registry.inspect(&created.id))
        .await
        .expect("inspect must not wait for foreground probe")
        .expect("inspect session");

    block.released.store(true, Ordering::Release);
    scan.await.expect("foreground rescan task");
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn procwatch_refreshes_agent_base_when_pid_is_reused() {
    let (registry, inspector, created) = mock_procwatch_registry("pid-reuse-base").await;
    inspector.set_descendants(
        created.pid,
        vec![codex_fact(PID_REUSE_AGENT_PID, created.pid)],
    );
    let first_scan = Instant::now();
    registry
        .rescan_procwatch_at(&created.id, created.pid, first_scan)
        .await;
    let first_seen = {
        let sessions = registry.inner.sessions.lock().await;
        let observed = sessions
            .get(&created.id)
            .expect("session entry")
            .observed_agents
            .iter()
            .find(|observed| observed.pid == PID_REUSE_AGENT_PID)
            .expect("observed codex pid");
        assert_eq!(observed.agent_base, RuntimeRef::codex());
        observed.first_seen
    };

    let mut replacement = claude_fact(PID_REUSE_AGENT_PID, created.pid);
    replacement.start_identity =
        crate::procwatch::StartIdentity::new(u64::from(PID_REUSE_AGENT_PID) + 1);
    replacement.pgid = PID_REUSE_AGENT_PID + 10;
    inspector.set_descendants(created.pid, vec![replacement]);
    let second_scan = first_seen + PID_REUSE_RESCAN_DELAY;
    registry
        .rescan_procwatch_at(&created.id, created.pid, second_scan)
        .await;
    assert_eq!(inspector.exit_watch_count(PID_REUSE_AGENT_PID), 2);

    {
        let sessions = registry.inner.sessions.lock().await;
        let observed = sessions
            .get(&created.id)
            .expect("session entry")
            .observed_agents
            .iter()
            .find(|observed| observed.pid == PID_REUSE_AGENT_PID)
            .expect("observed reused pid");
        assert_eq!(observed.agent_base, RuntimeRef::claude());
        assert_eq!(observed.start_identity, u64::from(PID_REUSE_AGENT_PID) + 1);
        assert_eq!(observed.pgid, PID_REUSE_AGENT_PID + 10);
        assert_eq!(observed.first_seen, second_scan);
    };

    registry
        .on_observed_agent_exit(
            &created.id,
            (PID_REUSE_AGENT_PID, u64::from(PID_REUSE_AGENT_PID)),
        )
        .await;
    let after_old_exit = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(after_old_exit.active_agent.as_deref(), Some("claude"));
    assert_eq!(after_old_exit.active_agent_pid, Some(PID_REUSE_AGENT_PID));

    inspector.set_descendants(created.pid, Vec::new());
    inspector.fire_exit_watch(PID_REUSE_AGENT_PID, 1);
    let after_new_exit = wait_for_active_agent_pid(&registry, &created.id, None).await;
    assert_eq!(after_new_exit.active_agent, None);

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn procwatch_reconciles_unbound_claim_after_descendants_error() {
    let (registry, inspector, created) = mock_procwatch_registry("ttl-descendants-error").await;
    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: CODEX_HOOK_SOURCE.to_owned(),
            agent: CODEX_HOOK_AGENT.to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(ReportSequence::new(DIRECT_AGENT_REPORT_SEQ)),
            pid: None,
            agent_session_id: Some("codex-unbound".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(report.recorded);
    let reported_at = {
        let sessions = registry.inner.sessions.lock().await;
        sessions
            .get(&created.id)
            .expect("session entry")
            .active_agent
            .as_ref()
            .expect("active report")
            .reported_at
    };

    inspector.fail_descendants_with(std::io::ErrorKind::Other);
    registry
        .rescan_procwatch_at(
            &created.id,
            created.pid,
            reported_at + Duration::from_millis(50),
        )
        .await;
    let cleared = registry.inspect(&created.id).await.expect("inspect");

    assert_eq!(cleared.active_agent, None);
    assert_eq!(cleared.active_agent_base, None);
    assert_eq!(cleared.active_agent_pid, None);
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn procwatch_updates_cwd_from_active_agent_pid() {
    let (registry, inspector, created) = mock_procwatch_registry("procwatch-cwd").await;
    let root_cwd = temp_dir("procwatch-root-cwd");
    let agent_cwd = temp_dir("procwatch-agent-cwd");

    inspector.set_cwd(created.pid, root_cwd.clone());
    inspector.set_descendants(
        created.pid,
        vec![codex_fact(RELEASE_MATRIX_AGENT_PID, created.pid)],
    );
    inspector.set_cwd(RELEASE_MATRIX_AGENT_PID, agent_cwd.clone());

    report_release_matrix_agent(&registry, &created).await;
    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;
    let updated = registry.inspect(&created.id).await.expect("inspect");

    assert_eq!(
        updated.cwd, agent_cwd,
        "procwatch must use the active agent pid as the cwd focus"
    );
    assert_ne!(
        updated.cwd, root_cwd,
        "root shell cwd must not win while an active agent pid is bound"
    );
    assert_eq!(updated.cwd_source, Some(CwdSource::Procwatch));

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn procwatch_skips_agents_owned_by_another_daemon_or_session() {
    // Regression: a nested daemon (a test-suite loopback instance, a
    // self-hosted dev run) spawns its own agent PTYs *inside* this session's
    // process subtree. Those processes look exactly like this session's agent
    // (`comm = codex`), but their ownership markers name the other daemon —
    // adopting one hijacked the session's active agent, cwd focus, and project
    // association. Only a process carrying this session's own markers (or no
    // markers at all) may be adopted.
    let (registry, inspector, created) = mock_procwatch_registry("foreign-agent").await;
    let root_cwd = temp_dir("foreign-agent-root-cwd");
    let agent_cwd = temp_dir("foreign-agent-agent-cwd");
    inspector.set_cwd(created.pid, root_cwd.clone());
    inspector.set_descendants(
        created.pid,
        vec![codex_fact(FOREIGN_AGENT_PID, created.pid)],
    );
    inspector.set_cwd(FOREIGN_AGENT_PID, agent_cwd.clone());

    // A foreign daemon's agent: never adopted, cwd focus stays on the root.
    inspector.set_ownership_markers(
        FOREIGN_AGENT_PID,
        OwnershipMarkers {
            daemon_id: Some("d-foreign".to_owned()),
            session_id: Some("s-1".to_owned()),
            worker_instance_id: None,
            runtime_id: None,
        },
    );
    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;
    let skipped = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(skipped.active_agent, None, "foreign daemon must be skipped");
    assert_eq!(skipped.active_agent_pid, None);
    assert_eq!(skipped.cwd, root_cwd, "cwd focus must stay on the root");
    let sessions = registry.inner.sessions.lock().await;
    assert!(
        sessions
            .get(&created.id)
            .expect("session entry")
            .observed_agents
            .is_empty(),
        "a foreign-owned process must not become an observed agent"
    );
    drop(sessions);

    // This daemon, but another session's agent: also skipped.
    inspector.set_ownership_markers(
        FOREIGN_AGENT_PID,
        OwnershipMarkers {
            daemon_id: Some(registry.daemon_instance_id().to_owned()),
            session_id: Some(format!("{}-other", created.id.0)),
            worker_instance_id: None,
            runtime_id: None,
        },
    );
    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;
    let sibling = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(
        sibling.active_agent, None,
        "sibling session must be skipped"
    );

    // The session's own agent (inherited markers) is adopted as before.
    inspector.set_ownership_markers(
        FOREIGN_AGENT_PID,
        OwnershipMarkers {
            daemon_id: Some(registry.daemon_instance_id().to_owned()),
            session_id: Some(created.id.0.clone()),
            worker_instance_id: None,
            runtime_id: None,
        },
    );
    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;
    let adopted = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(adopted.active_agent.as_deref(), Some("codex"));
    assert_eq!(adopted.active_agent_pid, Some(FOREIGN_AGENT_PID));
    assert_eq!(adopted.cwd, agent_cwd, "own agent drives the cwd focus");

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn external_rescan_skips_processes_marked_by_any_pohunek_daemon() {
    // A process carrying pohunek ownership markers is a PTY child of *some*
    // daemon instance — it is managed, not external, even when the owned-pid
    // tree walk cannot connect it to a local session (nested daemons, ppid
    // gaps). It must not surface as an external session.
    let inspector = Arc::new(MockInspector::default());
    let registry_inspector: Arc<dyn ProcessInspector> = Arc::<MockInspector>::clone(&inspector);
    let registry = SessionRegistry::new_with_inspector(
        SessionRegistryConfig {
            shell_command: hermetic_shell(),
            stop_grace: Duration::from_millis(50),
            ..SessionRegistryConfig::default()
        },
        registry_inspector,
    );
    inspector.set_descendants(1, vec![codex_fact(FOREIGN_AGENT_PID, 1)]);
    inspector.set_ownership_markers(
        FOREIGN_AGENT_PID,
        OwnershipMarkers {
            daemon_id: Some("d-foreign".to_owned()),
            session_id: None,
            worker_instance_id: None,
            runtime_id: None,
        },
    );

    registry
        .rescan_external_agents(&TranscriptIndex::default())
        .await;
    assert!(
        registry.list().await.is_empty(),
        "a marked process must not surface as an external session"
    );

    // The same process without markers is genuinely external and surfaces.
    inspector.set_ownership_markers(FOREIGN_AGENT_PID, OwnershipMarkers::default());
    registry
        .rescan_external_agents(&TranscriptIndex::default())
        .await;
    let sessions = registry.list().await;
    assert_eq!(sessions.len(), 1, "unmarked agent surfaces as external");
    assert_eq!(sessions[0].external, Some(true));
    assert_eq!(sessions[0].pid, FOREIGN_AGENT_PID);
}

#[tokio::test]
async fn external_immediate_exit_is_published_after_creation() {
    let inspector = Arc::new(MockInspector::default());
    let registry_inspector: Arc<dyn ProcessInspector> = Arc::<MockInspector>::clone(&inspector);
    let registry = SessionRegistry::new_with_inspector(
        SessionRegistryConfig {
            shell_command: hermetic_shell(),
            stop_grace: Duration::from_millis(50),
            ..SessionRegistryConfig::default()
        },
        registry_inspector,
    );
    inspector.set_descendants(1, vec![codex_fact(FOREIGN_AGENT_PID, 1)]);
    inspector.set_cwd(FOREIGN_AGENT_PID, temp_dir("external-immediate-exit"));
    inspector.set_immediate_exit(FOREIGN_AGENT_PID);
    let mut events = registry.subscribe();
    inspector.expect_created_before_exit_watch(FOREIGN_AGENT_PID, registry.subscribe());

    registry
        .rescan_external_agents(&TranscriptIndex::default())
        .await;

    let created = guard("the created event", events.recv())
        .await
        .expect("created event");
    assert_eq!(created.event(), protocol::event::SESSION_CREATED);
    let removed = guard("the removed event", events.recv())
        .await
        .expect("removed event");
    assert_eq!(removed.event(), protocol::event::SESSION_REMOVED);
    assert_eq!(
        created.payload()["session"]["id"],
        removed.payload()["session"]["id"]
    );
    assert!(
        registry.list().await.is_empty(),
        "an immediately exited external process must not remain observable"
    );
}

#[tokio::test]
async fn external_rescan_discards_a_candidate_that_changes_generation_before_upsert() {
    let inspector = Arc::new(MockInspector::default());
    let registry_inspector: Arc<dyn ProcessInspector> = Arc::<MockInspector>::clone(&inspector);
    let registry = SessionRegistry::new_with_inspector(
        SessionRegistryConfig {
            shell_command: hermetic_shell(),
            stop_grace: Duration::from_millis(50),
            ..SessionRegistryConfig::default()
        },
        registry_inspector,
    );
    let fact = codex_fact(FOREIGN_AGENT_PID, 1);
    inspector.set_descendants(1, vec![fact.clone()]);
    inspector.set_cwd(FOREIGN_AGENT_PID, temp_dir("external-reused-pid"));
    let association_block = Arc::new(ExternalAssociationBlock::default());
    *registry
        .inner
        .external_association_block
        .lock()
        .expect("external association test lock") = Some(Arc::clone(&association_block));
    let rescan_registry = registry.clone();
    let rescan = tokio::spawn(async move {
        rescan_registry
            .rescan_external_agents(&TranscriptIndex::default())
            .await;
    });
    association_block.entered.notified().await;
    inspector.set_identity(
        FOREIGN_AGENT_PID,
        Some(ProcessIdentity {
            pid: FOREIGN_AGENT_PID,
            start_identity: StartIdentity::new(fact.start_identity.get() + 1),
        }),
    );
    association_block.release.notify_one();
    rescan.await.expect("external rescan task");

    assert!(
        registry.list().await.is_empty(),
        "a stale process generation must not surface as an external session"
    );
    assert_eq!(
        inspector.exit_watch_count(FOREIGN_AGENT_PID),
        0,
        "a stale process generation must not arm an exit watch"
    );
}

#[tokio::test]
async fn external_rescan_preserves_verified_entry_on_identity_inspection_failure() {
    let inspector = Arc::new(MockInspector::default());
    let registry_inspector: Arc<dyn ProcessInspector> = Arc::<MockInspector>::clone(&inspector);
    let registry = SessionRegistry::new_with_inspector(
        SessionRegistryConfig {
            shell_command: hermetic_shell(),
            stop_grace: Duration::from_millis(50),
            ..SessionRegistryConfig::default()
        },
        registry_inspector,
    );
    inspector.set_descendants(1, vec![codex_fact(FOREIGN_AGENT_PID, 1)]);
    inspector.set_cwd(
        FOREIGN_AGENT_PID,
        temp_dir("external-identity-inspection-failure"),
    );
    registry
        .rescan_external_agents(&TranscriptIndex::default())
        .await;
    let verified = registry
        .list()
        .await
        .into_iter()
        .next()
        .expect("verified external session");

    inspector.fail_identity_with(FOREIGN_AGENT_PID, std::io::ErrorKind::Interrupted);
    registry
        .rescan_external_agents(&TranscriptIndex::default())
        .await;

    assert_eq!(registry.list().await, vec![verified]);
    assert_eq!(
        inspector.exit_watch_count(FOREIGN_AGENT_PID),
        1,
        "an inconclusive refresh must not replace the existing exit watch"
    );
}

#[tokio::test]
async fn session_read_rejects_external_observe_only_sessions() {
    let inspector = Arc::new(MockInspector::default());
    let registry_inspector: Arc<dyn ProcessInspector> = Arc::<MockInspector>::clone(&inspector);
    let registry = SessionRegistry::new_with_inspector(
        SessionRegistryConfig {
            shell_command: hermetic_shell(),
            stop_grace: Duration::from_millis(50),
            ..SessionRegistryConfig::default()
        },
        registry_inspector,
    );
    inspector.set_descendants(1, vec![codex_fact(FOREIGN_AGENT_PID, 1)]);
    inspector.set_cwd(FOREIGN_AGENT_PID, temp_dir("external-session-read"));

    registry
        .rescan_external_agents(&TranscriptIndex::default())
        .await;
    let external = registry.list().await.into_iter().next().expect("external");

    let params = SessionReadParams::new(external.id, None, None, None).expect("read params");
    let error = registry
        .session_read(&params)
        .await
        .expect_err("external sessions cannot be read");
    assert_eq!(error.code, "session_external_read_only");
    assert!(error
        .recover
        .as_deref()
        .is_some_and(|hint| hint.contains("pohunek")));
}

#[tokio::test]
async fn active_agent_release_matrix_covers_hook_fast_path_and_procwatch_backstop() {
    let (hook_registry, _hook_inspector, hook_created) =
        mock_procwatch_registry("hook-release-matrix").await;
    report_release_matrix_agent(&hook_registry, &hook_created).await;

    let release = hook_registry
        .release_agent(SessionReleaseAgentParams {
            session_id: hook_created.id.clone(),
            source: CODEX_HOOK_SOURCE.to_owned(),
            agent: CODEX_HOOK_AGENT.to_owned(),
            seq: Some(ReportSequence::new(RELEASE_MATRIX_RELEASE_SEQ)),
        })
        .await;
    assert!(release.released);
    let hook_cleared = hook_registry
        .inspect(&hook_created.id)
        .await
        .expect("inspect");
    assert_eq!(hook_cleared.active_agent, None);
    assert_eq!(hook_cleared.active_agent_base, None);
    assert_eq!(hook_cleared.active_agent_pid, None);
    assert_eq!(hook_cleared.active_agent_session_id, None);
    let _ = hook_registry.stop(&hook_created.id).await;

    let (backstop_registry, backstop_inspector, backstop_created) =
        mock_procwatch_registry("procwatch-release-matrix").await;
    backstop_inspector.set_descendants(
        backstop_created.pid,
        vec![codex_fact(RELEASE_MATRIX_AGENT_PID, backstop_created.pid)],
    );
    backstop_registry
        .rescan_procwatch_at(&backstop_created.id, backstop_created.pid, Instant::now())
        .await;
    report_release_matrix_agent(&backstop_registry, &backstop_created).await;

    backstop_inspector.set_descendants(backstop_created.pid, Vec::new());
    backstop_inspector.fire_exit(RELEASE_MATRIX_AGENT_PID);
    let backstop_cleared =
        wait_for_active_agent_pid(&backstop_registry, &backstop_created.id, None).await;
    assert_eq!(backstop_cleared.active_agent, None);
    assert_eq!(backstop_cleared.active_agent_base, None);
    assert_eq!(backstop_cleared.active_agent_session_id, None);
    let _ = backstop_registry.stop(&backstop_created.id).await;
}

#[tokio::test]
async fn procwatch_releases_hook_claim_when_observed_pid_exits() {
    let (registry, inspector, created) = mock_procwatch_registry("hook-exit").await;
    inspector.set_descendants(created.pid, vec![codex_fact(100, created.pid)]);
    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;

    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(ReportSequence::new(10)),
            pid: Some(100),
            agent_session_id: Some("codex-native".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(report.recorded);
    assert_eq!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect")
            .active_agent_pid,
        Some(100)
    );

    inspector.set_descendants(created.pid, Vec::new());
    inspector.fire_exit(100);
    let cleared = wait_for_active_agent_pid(&registry, &created.id, None).await;

    assert_eq!(cleared.active_agent, None);
    assert_eq!(cleared.active_agent_base, None);
    assert_eq!(cleared.active_agent_session_id, None);
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn procwatch_late_hook_report_for_dead_pid_is_cleared_on_reconcile() {
    let (registry, inspector, created) = mock_procwatch_registry("late-hook").await;
    inspector.set_descendants(created.pid, vec![codex_fact(100, created.pid)]);
    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;
    inspector.set_descendants(created.pid, Vec::new());
    inspector.fire_exit(100);
    wait_for_active_agent_pid(&registry, &created.id, None).await;

    let late = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(ReportSequence::new(20)),
            pid: Some(100),
            agent_session_id: Some("late-native".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(late.recorded);

    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;
    let cleared = registry.inspect(&created.id).await.expect("inspect");

    assert_eq!(cleared.active_agent, None);
    assert_eq!(cleared.active_agent_pid, None);
    assert_eq!(cleared.active_agent_session_id, None);
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn procwatch_auto_report_releases_immediately_on_sigkill_style_exit() {
    let (registry, inspector, created) = mock_procwatch_registry("sigkill").await;
    inspector.set_descendants(created.pid, vec![codex_fact(100, created.pid)]);
    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;
    let active = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(active.active_agent.as_deref(), Some("codex"));
    assert_eq!(active.active_agent_base, Some(RuntimeRef::codex()));
    assert_eq!(active.active_agent_pid, Some(100));

    inspector.set_descendants(created.pid, Vec::new());
    inspector.fire_exit(100);
    let cleared = wait_for_active_agent_pid(&registry, &created.id, None).await;

    assert_eq!(cleared.active_agent, None);
    assert_eq!(cleared.active_agent_base, None);
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn procwatch_expires_unbound_hook_claim_after_ttl() {
    let (registry, _inspector, created) = mock_procwatch_registry("ttl").await;
    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(ReportSequence::new(30)),
            pid: None,
            agent_session_id: Some("codex-native".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(report.recorded);
    let reported_at = {
        let sessions = registry.inner.sessions.lock().await;
        sessions
            .get(&created.id)
            .expect("session entry")
            .active_agent
            .as_ref()
            .expect("active report")
            .reported_at
    };

    registry
        .rescan_procwatch_at(
            &created.id,
            created.pid,
            reported_at + Duration::from_millis(49),
        )
        .await;
    assert_eq!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect")
            .active_agent_pid,
        None
    );
    assert_eq!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect")
            .active_agent
            .as_deref(),
        Some("codex")
    );

    registry
        .rescan_procwatch_at(
            &created.id,
            created.pid,
            reported_at + Duration::from_millis(50),
        )
        .await;
    let cleared = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(cleared.active_agent, None);
    assert_eq!(cleared.active_agent_pid, None);
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn procwatch_rebinds_restart_to_new_observed_pid() {
    let (registry, inspector, created) = mock_procwatch_registry("restart").await;
    inspector.set_descendants(created.pid, vec![codex_fact(100, created.pid)]);
    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;
    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(ReportSequence::new(40)),
            pid: Some(100),
            agent_session_id: Some("old-native".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(report.recorded);

    inspector.set_descendants(created.pid, vec![codex_fact(200, created.pid)]);
    registry
        .rescan_procwatch_at(
            &created.id,
            created.pid,
            Instant::now() + Duration::from_secs(1),
        )
        .await;
    let rebound = registry.inspect(&created.id).await.expect("inspect");

    assert_eq!(rebound.active_agent.as_deref(), Some("codex"));
    assert_eq!(rebound.active_agent_base, Some(RuntimeRef::codex()));
    assert_eq!(rebound.active_agent_pid, Some(200));
    assert_eq!(rebound.active_agent_session_id, None);
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn report_native_id_records_binding_and_updates_info() {
    let store_path = temp_store_path("report");
    let agents_dir = temp_resumable_agents_dir("report");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(resumable_params())
        .await
        .expect("create session");
    assert_eq!(created.native_session_id, None);

    let result = registry
        .report_native_id(native_report!(&registry;
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "native-abc".to_owned(),
            transcript_path: None,
        ))
        .await;
    assert!(result.recorded);

    // In-memory info now carries the native id.
    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(inspected.native_session_id.as_deref(), Some("native-abc"));

    // The binding was persisted to the store.
    let persisted = crate::store::Store::new(store_path.clone())
        .load_resume()
        .expect("load store");
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].session_id, created.id.0);
    assert_eq!(
        persisted[0].native_session_id.as_deref(),
        Some("native-abc")
    );
    let sessions = crate::store::Store::new(store_path)
        .load_sessions()
        .expect("reload durable sessions");
    let ordering = sessions[0]
        .native_identity_ordering
        .as_ref()
        .expect("native identity ordering persisted");
    assert!(
        !native_report_is_current(
            Some(ordering),
            &ordering.worker_instance_id,
            ordering.sequence - 1
        ),
        "a lower sequence must remain stale after the ordering key is reloaded"
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn concurrent_session_writes_cannot_regress_native_ordering() {
    let store_path = temp_store_path("native-ordering-concurrent");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(temp_resumable_agents_dir("native-ordering-concurrent")),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(resumable_params())
        .await
        .expect("create session");
    assert!(
        registry
            .report_native_id(native_report!(&registry;
                session_id: created.id.clone(),
                agent: "claude".to_owned(),
                native_session_id: "native-initial".to_owned(),
                transcript_path: None,
            ))
            .await
            .recorded
    );
    let store = Arc::new(crate::store::Store::new(store_path));
    let base = store
        .load_sessions()
        .expect("load base record")
        .pop()
        .expect("base session record");
    let instance_id = base
        .runtime
        .worker_instance_id
        .clone()
        .expect("runtime identity");
    let older = native_ordering_record(
        &base,
        &instance_id,
        created.pid,
        u64::MAX - 1,
        "native-older",
    );
    let newer = native_ordering_record(&base, &instance_id, created.pid, u64::MAX, "native-newer");
    write_newer_then_older(Arc::clone(&store), older.clone(), newer);
    let collision = native_ordering_record(
        &base,
        &instance_id,
        created.pid,
        u64::MAX,
        "native-collision",
    );
    store
        .record_session(&collision)
        .expect("attempt equal-sequence identity collision");
    store
        .record_resume(collision.recovery.as_ref().expect("collision recovery"))
        .expect("attempt equal-sequence resume collision");

    let mut delayed_resize = older;
    delayed_resize.info.cols = 132;
    store
        .record_session(&delayed_resize)
        .expect("write delayed resize snapshot");
    let persisted = store
        .load_sessions()
        .expect("reload ordered record")
        .pop()
        .expect("persisted session");
    assert_eq!(persisted.info.cols, 132);
    assert_eq!(
        persisted
            .native_identity_ordering
            .as_ref()
            .expect("persisted ordering")
            .sequence,
        u64::MAX
    );
    assert_eq!(
        persisted.info.native_session_id.as_deref(),
        Some("native-newer")
    );
    assert_eq!(
        store
            .load_resume()
            .expect("reload ordered resume")
            .pop()
            .and_then(|binding| binding.native_session_id),
        Some("native-newer".to_owned())
    );
    assert_eq!(
        persisted
            .recovery
            .as_ref()
            .and_then(|binding| binding.native_session_id.as_deref()),
        Some("native-newer")
    );

    assert_previous_runtime_cannot_overwrite(&store, &persisted, &delayed_resize);
    let _ = registry.stop(&created.id).await;
}

fn assert_previous_runtime_cannot_overwrite(
    store: &crate::store::Store,
    persisted: &crate::store::SessionRecord,
    delayed: &crate::store::SessionRecord,
) {
    let mut next_runtime = persisted.clone();
    let next_generation = next_runtime
        .info
        .runtime
        .as_ref()
        .expect("runtime projection")
        .runtime_generation
        .get()
        + 1;
    let runtime = next_runtime
        .info
        .runtime
        .as_mut()
        .expect("runtime projection");
    runtime.runtime_generation = RuntimeGeneration::new(next_generation);
    runtime.worker_instance_id = Some("runtime-next".to_owned());
    next_runtime.runtime.worker_instance_id = Some("runtime-next".to_owned());
    next_runtime.native_identity_ordering = None;
    next_runtime.info.cols = 144;
    store
        .record_session(&next_runtime)
        .expect("write next runtime generation");
    store
        .record_resume(next_runtime.recovery.as_ref().expect("next recovery"))
        .expect("write next runtime resume");
    store
        .record_session(delayed)
        .expect("attempt previous-runtime physical write");
    store
        .record_resume(delayed.recovery.as_ref().expect("older recovery"))
        .expect("attempt previous-runtime resume write");
    let persisted = store
        .load_sessions()
        .expect("reload next runtime record")
        .pop()
        .expect("persisted next runtime");
    let runtime = persisted.info.runtime.as_ref().expect("persisted runtime");
    assert_eq!(
        runtime.runtime_generation,
        RuntimeGeneration::new(next_generation)
    );
    assert_eq!(runtime.worker_instance_id.as_deref(), Some("runtime-next"));
    assert_eq!(
        persisted.runtime.worker_instance_id.as_deref(),
        Some("runtime-next")
    );
    assert_eq!(persisted.info.cols, 144);
    assert_eq!(
        store
            .load_resume()
            .expect("reload next runtime resume")
            .pop()
            .and_then(|binding| binding.native_session_id),
        Some("native-newer".to_owned())
    );

    let base = persisted;
    let mut newer_same_runtime = base.clone();
    newer_same_runtime.info.cols = 155;
    store
        .record_session(&newer_same_runtime)
        .expect("persist newer same-runtime resize");
    let mut stale_terminal = base.clone();
    stale_terminal.info.state = SessionState::Done;
    stale_terminal.info.runtime.as_mut().expect("runtime").state = RuntimeState::Terminal;
    stale_terminal.runtime.state = RuntimeState::Terminal;
    assert_eq!(
        store
            .record_session_if_current(&base, &stale_terminal)
            .expect("reject stale conditional transition"),
        crate::store::SessionWriteOutcome::StaleSnapshot
    );
    let durable = store
        .load_sessions()
        .expect("reload after rejected conditional transition")
        .pop()
        .expect("durable record");
    assert_eq!(durable.info.cols, 155);
    assert_eq!(durable.info.state, SessionState::Running);
}

#[tokio::test]
async fn concurrent_equal_generation_commits_publish_only_the_durable_winner() {
    let store_path = temp_store_path("runtime-commit-race");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create base session");
    registry.stop(&created.id).await.expect("stop base session");
    let base = registry
        .inner
        .sessions
        .lock()
        .await
        .remove(&created.id)
        .expect("remove base registry entry");
    let candidate_a = runtime_commit_candidate(base.clone(), "runtime-a");
    let candidate_b = runtime_commit_candidate(base, "runtime-b");
    let mut preparing = SessionRegistry::session_record(
        &created.id,
        &candidate_a,
        crate::store::DesiredState::Running,
        None,
    );
    preparing.info.state = SessionState::Starting;
    preparing
        .info
        .runtime
        .as_mut()
        .expect("preparing runtime")
        .worker_instance_id = None;
    preparing.runtime.worker_instance_id = None;
    let store = crate::store::Store::new(store_path);
    assert_eq!(
        store.record_session(&preparing).expect("persist preparing"),
        crate::store::SessionWriteOutcome::Applied
    );
    registry
        .inner
        .store
        .as_ref()
        .expect("registry store")
        .fail_next_parent_sync_after_rename();

    let mut events = registry.subscribe();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let first = commit_runtime_candidate(
        registry.clone(),
        Arc::clone(&barrier),
        created.id.clone(),
        candidate_a,
    );
    let second =
        commit_runtime_candidate(registry.clone(), barrier, created.id.clone(), candidate_b);
    let (first, second) = tokio::join!(first, second);
    let winner = match (first, second) {
        (Ok(winner), Err(error)) | (Err(error), Ok(winner)) => {
            assert_eq!(error.code, "session_runtime_commit_stale");
            winner
        }
        outcomes => panic!("exactly one runtime commit must win: {outcomes:?}"),
    };
    assert_runtime_commit_winner(&registry, &store, &created.id, &winner).await;
    let event = guard("the winner event", events.recv())
        .await
        .expect("winner event");
    assert_eq!(event.event(), protocol::event::SESSION_UPDATED);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err(),
        "losing runtime must not publish a success event"
    );
}

#[tokio::test]
async fn stale_runtime_watchers_emit_nothing_during_new_runtime_commit() {
    let store_path = temp_store_path("stale-watcher-commit-window");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        // The foreground process dies on SIGTERM. The default command is the host's
        // interactive `$SHELL`, which ignores it, so stop would wait out the grace and
        // then depend on the SIGKILL window.
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        store_path: Some(store_path),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create base session");
    registry.stop(&created.id).await.expect("stop base session");
    let base = registry
        .inner
        .sessions
        .lock()
        .await
        .remove(&created.id)
        .expect("remove base registry entry");
    let old_runtime = runtime_commit_candidate_for_generation(base.clone(), "runtime-old", 1);
    let expected_old =
        super::RuntimeWatchIdentity::from_info(&old_runtime.info).expect("old watcher identity");
    let new_runtime = runtime_commit_candidate(base, "runtime-new");
    let new_record = SessionRegistry::session_record(
        &created.id,
        &new_runtime,
        crate::store::DesiredState::Running,
        None,
    );
    // The commit goes through the registry's own store: its write lock is what
    // serializes it against a stale watcher's in-flight write. A second `Store`
    // over the same path has its own lock and can lose that update.
    let store = registry.inner.store.clone().expect("registry store");
    assert_eq!(
        store
            .record_session(&new_record)
            .expect("commit new runtime"),
        crate::store::SessionWriteOutcome::Applied
    );
    registry
        .inner
        .sessions
        .lock()
        .await
        .insert(created.id.clone(), new_runtime);
    let mut events = registry.subscribe();

    let _ = registry
        .record_exit(
            &created.id,
            RuntimeExit {
                exit_code: Some(0),
                success: true,
            },
            false,
            Some(&expected_old),
            None,
        )
        .await;
    registry
        .mark_worker_unavailable(
            &created.id,
            &expected_old,
            RuntimeState::Lost,
            crate::session::supervision::RUNTIME_LOST,
        )
        .await;
    assert!(
        !registry
            .mark_worker_reconnecting(
                &created.id,
                &expected_old,
                &WorkerError::Protocol("test reconnect".to_owned()),
            )
            .await,
        "stale reconnect callback must stop its watcher"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err(),
        "stale runtime watchers must not publish lifecycle events"
    );

    assert_runtime_commit_winner(&registry, &store, &created.id, "runtime-new").await;
}

#[tokio::test]
async fn reconnect_rejects_a_replacement_worker_before_mutating_registry() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(temp_store_path("reconnect-identity-mismatch")),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    let (worker, original) = {
        let mut sessions = registry.inner.sessions.lock().await;
        let entry = sessions.get_mut(&created.id).expect("live entry");
        let RuntimeHandle::Worker(worker) = &entry.runtime else {
            panic!("worker runtime");
        };
        let original = entry.info.runtime.clone().expect("runtime info");
        entry
            .info
            .runtime
            .as_mut()
            .expect("runtime info")
            .worker_instance_id = Some("runtime-old".to_owned());
        (worker.clone(), original)
    };
    let expected = {
        let sessions = registry.inner.sessions.lock().await;
        super::RuntimeWatchIdentity::from_info(&sessions[&created.id].info)
            .expect("old runtime identity")
    };
    let mut events = registry.subscribe();

    let outcome = registry
        .adopt_reconnected_worker(&created.id, &expected, worker)
        .await;

    assert!(matches!(
        outcome,
        super::RuntimeTransitionOutcome::IdentityMismatch
    ));
    assert_eq!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect unchanged entry")
            .runtime
            .and_then(|runtime| runtime.worker_instance_id),
        Some("runtime-old".to_owned())
    );
    assert_no_runtime_event(&mut events).await;
    registry
        .inner
        .sessions
        .lock()
        .await
        .get_mut(&created.id)
        .expect("restore live entry")
        .info
        .runtime = Some(original);
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn reconnect_persistence_failure_retries_without_orphaning_live_handle() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(temp_store_path("reconnect-persist-retry")),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    let (worker, expected) = live_worker_and_identity(&registry, &created.id).await;
    assert!(
        registry
            .mark_worker_reconnecting(
                &created.id,
                &expected,
                &WorkerError::Protocol("test disconnect".to_owned()),
            )
            .await
    );
    let mut events = registry.subscribe();
    registry
        .inner
        .store
        .as_ref()
        .expect("registry store")
        .fail_next_write_before_rename();

    let failed = registry
        .adopt_reconnected_worker(&created.id, &expected, worker.clone())
        .await;
    assert!(matches!(
        failed,
        super::RuntimeTransitionOutcome::RetryablePersistenceFailure(_)
    ));
    assert_eq!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect reconnecting entry")
            .runtime
            .expect("runtime")
            .state,
        RuntimeState::Reconnecting
    );
    assert_reconnect_transition_applied(&registry, &created.id, &expected, &worker).await;
    assert_eq!(
        next_runtime_event(&mut events).await,
        protocol::event::SESSION_RUNTIME_RECONNECTED
    );

    assert!(
        registry
            .mark_worker_reconnecting(
                &created.id,
                &expected,
                &WorkerError::Protocol("test second disconnect".to_owned()),
            )
            .await
    );
    registry
        .inner
        .store
        .as_ref()
        .expect("registry store")
        .fail_next_parent_sync_after_rename();
    assert_reconnect_transition_applied(&registry, &created.id, &expected, &worker).await;
    assert_eq!(
        next_runtime_event(&mut events).await,
        protocol::event::SESSION_RUNTIME_RECONNECTED
    );
    let _ = registry.stop(&created.id).await;
}

/// Connect deadline of the unanswered-socket reconnect attempt; short so the
/// timed-out attempt reaches classification quickly. The healthy session keeps
/// the registry's default launch budget.
const UNANSWERED_CONNECT_DEADLINE: Duration = Duration::from_millis(300);
/// Bound on the whole unanswered-socket reconnect. It only fails a hung loop:
/// a correct loop returns after about one connect deadline.
const UNANSWERED_RECONNECT_BOUND: Duration = Duration::from_secs(20);

#[tokio::test]
async fn reconnect_classifies_a_worker_socket_that_accepts_and_never_answers() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(temp_store_path("reconnect-unanswered")),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    let (_worker, expected) = live_worker_and_identity(&registry, &created.id).await;
    let socket = temp_dir("unanswered").join("w.sock");
    let listener = tokio::net::UnixListener::bind(&socket).expect("bind unanswered socket");
    let (accepted_tx, mut accepted_rx) = tokio::sync::mpsc::unbounded_channel();
    // Accepts every connection and keeps it open without ever writing, so a
    // connect attempt stays pending in negotiation.
    let silent = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
            if accepted_tx.send(()).is_err() {
                break;
            }
        }
        held
    });

    let started = Instant::now();
    let reconnected = tokio::time::timeout(
        UNANSWERED_RECONNECT_BOUND,
        registry.reconnect_worker(
            &created.id,
            &expected,
            &socket,
            &tokio_util::sync::CancellationToken::new(),
            &WorkerError::Protocol("test disconnect".to_owned()),
            UNANSWERED_CONNECT_DEADLINE,
        ),
    )
    .await
    .expect("the reconnect loop classifies an unanswered worker instead of hanging");

    assert!(reconnected.is_none(), "nothing is adopted");
    assert!(
        started.elapsed() >= UNANSWERED_CONNECT_DEADLINE,
        "an unproven loss is classified only after the connect deadline"
    );
    assert!(
        accepted_rx.try_recv().is_ok(),
        "the attempt reached a socket that accepted it"
    );
    let runtime = registry
        .inspect(&created.id)
        .await
        .expect("session stays visible")
        .runtime
        .expect("runtime");
    assert_eq!(runtime.state, RuntimeState::Conflict);
    // The worker that served the session is still running, so the loss is
    // unproven and the runtime waits in conflict for the supervision retry.
    assert_eq!(
        runtime.loss_reason.as_deref(),
        Some(crate::runtime::lifecycle::SUPERVISION_AMBIGUOUS)
    );
    silent.abort();
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn lost_transition_retries_precommit_failure_before_single_event() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(temp_store_path("lost-persist-retry")),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    let (worker, expected) = live_worker_and_identity(&registry, &created.id).await;
    assert!(
        registry
            .mark_worker_reconnecting(
                &created.id,
                &expected,
                &WorkerError::Protocol("test disconnect".to_owned()),
            )
            .await
    );
    registry
        .inner
        .sessions
        .lock()
        .await
        .get_mut(&created.id)
        .expect("session entry")
        .info
        .subagents
        .push(running_subagent("child-lost", 8));
    let mut events = registry.subscribe();
    registry
        .inner
        .store
        .as_ref()
        .expect("registry store")
        .fail_next_write_before_rename();

    assert!(matches!(
        registry
            .mark_worker_unavailable(
                &created.id,
                &expected,
                RuntimeState::Lost,
                crate::session::supervision::RUNTIME_LOST,
            )
            .await,
        super::RuntimeTransitionOutcome::RetryablePersistenceFailure(_)
    ));
    assert_eq!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect retryable lost")
            .runtime
            .expect("runtime")
            .state,
        RuntimeState::Reconnecting
    );
    assert_lost_transition_applied(&registry, &created.id, &expected).await;
    let inspected = registry.inspect(&created.id).await.expect("inspect lost");
    let subagent = inspected.subagents.first().expect("lost subagent");
    assert_eq!(subagent.lifecycle, SubagentLifecycle::Lost);
    assert!(subagent.revision.get() > 8);
    assert_eq!(
        next_runtime_event(&mut events).await,
        protocol::event::SESSION_RUNTIME_LOST
    );
    assert_no_runtime_event(&mut events).await;
    stop_test_worker(worker).await;
}

#[tokio::test]
async fn exit_transition_retries_precommit_failure_before_single_event() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(temp_store_path("exit-persist-retry")),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    let (worker, expected) = live_worker_and_identity(&registry, &created.id).await;
    let mut events = registry.subscribe();
    registry
        .inner
        .store
        .as_ref()
        .expect("registry store")
        .fail_next_write_before_rename();
    let exit = RuntimeExit {
        exit_code: Some(0),
        success: true,
    };

    let error = registry
        .record_exit(&created.id, exit, false, Some(&expected), None)
        .await
        .expect_err("precommit exit failure");
    assert_eq!(error.code, "session_store_failed");
    assert_eq!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect retryable exit")
            .state,
        SessionState::Running
    );
    assert_exit_transition_committed(
        &registry,
        &created.id,
        exit,
        &expected,
        "retry exit transition",
    )
    .await;
    assert_eq!(
        next_runtime_event(&mut events).await,
        protocol::event::SESSION_UPDATED
    );
    assert_no_runtime_event(&mut events).await;
    stop_test_worker(worker).await;
}

#[tokio::test]
async fn stop_retries_terminal_write_failure_before_canceling_watcher() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(temp_store_path("stop-terminal-persist-retry")),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    let mut events = registry.subscribe();
    registry
        .inner
        .store
        .as_ref()
        .expect("registry store")
        .fail_write_before_rename_after(2);

    assert!(
        registry
            .stop(&created.id)
            .await
            .expect("retry transient terminal commit failure")
            .stopped
    );
    assert_eq!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect stopped session")
            .state,
        SessionState::Stopped
    );
    assert_eq!(
        next_runtime_event(&mut events).await,
        protocol::event::SESSION_STOPPED
    );
    assert_no_runtime_event(&mut events).await;
}

#[tokio::test]
async fn spontaneous_exit_uses_durable_base_after_uncaptured_resize() {
    let store_path = temp_store_path("exit-after-uncaptured-resize");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    let (worker, _) = live_worker_and_identity(&registry, &created.id).await;
    registry
        .resize(&created.id, 137, 47)
        .await
        .expect("resize uncaptured session");
    let store = crate::store::Store::new(store_path);
    let mut durable_before_exit = store
        .load_sessions()
        .expect("load pre-exit session")
        .into_iter()
        .find(|record| record.session_id == created.id.0)
        .expect("durable pre-exit session");
    let worker_instance_id = durable_before_exit
        .runtime
        .worker_instance_id
        .clone()
        .expect("durable runtime id");
    durable_before_exit.info.native_session_id = Some("durable-native-newer".to_owned());
    durable_before_exit.info.native_session_path = Some("/work/durable-native-newer".to_owned());
    durable_before_exit.native_identity_ordering = Some(crate::store::NativeIdentityOrdering {
        worker_instance_id,
        pid: 4242,
        pid_start_identity: 777,
        sequence: 9,
    });
    let recovery = durable_before_exit
        .recovery
        .as_mut()
        .expect("durable recovery snapshot");
    recovery.native_session_id = durable_before_exit.info.native_session_id.clone();
    recovery.native_session_path = durable_before_exit.info.native_session_path.clone();
    assert!(matches!(
        store
            .record_session(&durable_before_exit)
            .expect("persist newer durable native identity"),
        crate::store::SessionWriteOutcome::Applied
    ));
    let mut events = registry.subscribe();

    stop_test_worker(worker).await;

    wait_until("the exit watcher terminal commit", || async {
        let state = registry
            .inspect(&created.id)
            .await
            .expect("inspect exited session")
            .state;
        state.is_terminal().then_some(())
    })
    .await;
    let durable = store
        .load_sessions()
        .expect("load terminal session")
        .into_iter()
        .find(|record| record.session_id == created.id.0)
        .expect("durable session");
    assert!(durable.info.state.is_terminal());
    assert_eq!(durable.runtime.state, RuntimeState::Terminal);
    assert_eq!((durable.info.cols, durable.info.rows), (137, 47));
    assert_eq!(
        durable.info.native_session_id.as_deref(),
        Some("durable-native-newer")
    );
    assert_eq!(
        durable.info.native_session_path.as_deref(),
        Some("/work/durable-native-newer")
    );
    let registry_info = registry
        .inspect(&created.id)
        .await
        .expect("inspect committed terminal session");
    // Process observation may publish a live update (e.g. the procwatch cwd)
    // before the exit commit; the terminal update is the one under test.
    let event_info = loop {
        let info = next_session_updated(&mut events).await;
        if info.state.is_terminal() {
            break info;
        }
    };
    assert_eq!(registry_info, durable.info);
    assert_eq!(event_info, durable.info);
    assert_no_runtime_event(&mut events).await;
}

async fn live_worker_and_identity(
    registry: &SessionRegistry,
    id: &SessionId,
) -> (Worker, RuntimeWatchIdentity) {
    let sessions = registry.inner.sessions.lock().await;
    let entry = sessions.get(id).expect("live entry");
    let RuntimeHandle::Worker(worker) = &entry.runtime else {
        panic!("worker runtime");
    };
    (
        worker.clone(),
        RuntimeWatchIdentity::from_info(&entry.info).expect("watch identity"),
    )
}

async fn assert_reconnect_transition_applied(
    registry: &SessionRegistry,
    id: &SessionId,
    expected: &RuntimeWatchIdentity,
    worker: &Worker,
) {
    // A live worker task may commit a same-runtime snapshot between the durable
    // write and memory compare. Production retries this explicit outcome.
    for _ in 0..CONCURRENT_TRANSITION_RETRY_LIMIT {
        match registry
            .adopt_reconnected_worker(id, expected, worker.clone())
            .await
        {
            super::RuntimeTransitionOutcome::Applied(_) => return,
            super::RuntimeTransitionOutcome::RetryableConcurrentChange => {
                tokio::task::yield_now().await;
            }
            outcome => panic!("unexpected reconnect retry outcome: {outcome:?}"),
        }
    }
    panic!("reconnect retry remained concurrently stale");
}

async fn assert_lost_transition_applied(
    registry: &SessionRegistry,
    id: &SessionId,
    expected: &RuntimeWatchIdentity,
) {
    for _ in 0..CONCURRENT_TRANSITION_RETRY_LIMIT {
        match registry
            .mark_worker_unavailable(
                id,
                expected,
                RuntimeState::Lost,
                crate::session::supervision::RUNTIME_LOST,
            )
            .await
        {
            super::RuntimeTransitionOutcome::Applied(_) => return,
            super::RuntimeTransitionOutcome::RetryableConcurrentChange => {
                tokio::task::yield_now().await;
            }
            outcome => panic!("unexpected lost retry outcome: {outcome:?}"),
        }
    }
    panic!("lost retry remained concurrently stale");
}

/// Commits an exit transition while a live worker task is running.
///
/// `record_exit` reports `Ok(false)` when a concurrent same-runtime snapshot
/// commit lands between its durable read and its commit
/// (`RuntimeTransitionOutcome::RetryableConcurrentChange`), which production
/// callers retry. Mirrors `assert_reconnect_transition_applied`.
async fn assert_exit_transition_committed(
    registry: &SessionRegistry,
    id: &SessionId,
    exit: RuntimeExit,
    expected: &RuntimeWatchIdentity,
    context: &str,
) {
    for _ in 0..CONCURRENT_TRANSITION_RETRY_LIMIT {
        match registry
            .record_exit(id, exit, false, Some(expected), None)
            .await
        {
            Ok(true) => return,
            Ok(false) => tokio::task::yield_now().await,
            Err(error) => panic!("{context}: unexpected exit error: {error}"),
        }
    }
    panic!("{context}: exit transition remained concurrently stale");
}

async fn next_runtime_event(events: &mut tokio::sync::broadcast::Receiver<Event>) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("runtime event timeout")
            .expect("runtime event");
        if is_runtime_transition_event(event.event()) {
            return event.event().to_owned();
        }
    }
}

async fn next_agent_state_event(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
) -> protocol::AgentStateEvent {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("agent state event timeout")
            .expect("agent state event");
        if event.event() == protocol::event::AGENT_STATE {
            return serde_json::from_value(event.payload().clone())
                .expect("valid agent state event");
        }
    }
}

async fn assert_no_runtime_event(events: &mut tokio::sync::broadcast::Receiver<Event>) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(50);
    loop {
        let Ok(event) = tokio::time::timeout_at(deadline, events.recv()).await else {
            return;
        };
        let event = event.expect("runtime event channel");
        assert!(
            !is_runtime_transition_event(event.event()),
            "unexpected runtime event: {event:?}"
        );
    }
}

fn is_runtime_transition_event(name: &str) -> bool {
    matches!(
        name,
        protocol::event::SESSION_RUNTIME_RECONNECTED
            | protocol::event::SESSION_RUNTIME_LOST
            | protocol::event::SESSION_UPDATED
            | protocol::event::SESSION_STOPPED
    )
}

async fn stop_test_worker(worker: crate::runtime::Worker) {
    let transaction =
        pohunek_worker_protocol::TransactionId::new("test-cleanup").expect("cleanup transaction");
    let _ = worker.stop(transaction).await;
}

fn runtime_commit_candidate(entry: SessionEntry, worker_instance_id: &str) -> SessionEntry {
    runtime_commit_candidate_for_generation(entry, worker_instance_id, 2)
}

fn runtime_commit_candidate_for_generation(
    mut entry: SessionEntry,
    worker_instance_id: &str,
    generation: u64,
) -> SessionEntry {
    entry.info.state = SessionState::Running;
    let runtime = entry.info.runtime.as_mut().expect("candidate runtime");
    runtime.state = RuntimeState::Live;
    runtime.runtime_generation = RuntimeGeneration::new(generation);
    runtime.worker_instance_id = Some(worker_instance_id.to_owned());
    entry.runtime = RuntimeHandle::Unavailable(RuntimeState::Live);
    entry.last_native_report = None;
    entry
}

async fn commit_runtime_candidate(
    registry: SessionRegistry,
    barrier: Arc<tokio::sync::Barrier>,
    id: SessionId,
    entry: SessionEntry,
) -> Result<String, protocol::ProtocolError> {
    let worker_instance_id = entry
        .info
        .runtime
        .as_ref()
        .and_then(|runtime| runtime.worker_instance_id.clone())
        .expect("candidate runtime id");
    let info = entry.info.clone();
    barrier.wait().await;
    registry.commit_session_entry(&id, entry).await?;
    registry.emit(protocol::event::SESSION_UPDATED, &info);
    Ok(worker_instance_id)
}

async fn assert_runtime_commit_winner(
    registry: &SessionRegistry,
    store: &crate::store::Store,
    id: &SessionId,
    winner: &str,
) {
    let memory = registry.inspect(id).await.expect("inspect durable winner");
    let durable = store
        .load_sessions()
        .expect("load durable winner")
        .into_iter()
        .find(|record| record.session_id == id.0)
        .expect("durable winner record");
    assert_eq!(
        memory
            .runtime
            .and_then(|runtime| runtime.worker_instance_id),
        Some(winner.to_owned())
    );
    assert_eq!(durable.runtime.worker_instance_id.as_deref(), Some(winner));
    registry
        .write_session_record(durable)
        .await
        .expect("retry exact committed runtime");
}

fn native_ordering_record(
    base: &crate::store::SessionRecord,
    worker_instance_id: &str,
    pid: u32,
    sequence: u64,
    native: &str,
) -> crate::store::SessionRecord {
    let mut record = base.clone();
    record.native_identity_ordering = Some(crate::store::NativeIdentityOrdering {
        worker_instance_id: worker_instance_id.to_owned(),
        pid,
        pid_start_identity: 1,
        sequence,
    });
    record.info.native_session_id = Some(native.to_owned());
    record
        .recovery
        .as_mut()
        .expect("recovery binding")
        .native_session_id = Some(native.to_owned());
    record
}

fn write_newer_then_older(
    store: Arc<crate::store::Store>,
    older: crate::store::SessionRecord,
    newer: crate::store::SessionRecord,
) {
    let older_resume = older.recovery.clone().expect("older recovery");
    let newer_resume = newer.recovery.clone().expect("newer recovery");
    let barrier = Arc::new(Barrier::new(2));
    let (newer_written, wait_for_newer) = std::sync::mpsc::channel();
    let older_store = Arc::clone(&store);
    let older_barrier = Arc::clone(&barrier);
    let older_write = std::thread::spawn(move || {
        older_barrier.wait();
        wait_for_newer.recv().expect("newer write completed");
        older_store
            .record_session(&older)
            .expect("attempt stale physical write");
        older_store
            .record_resume(&older_resume)
            .expect("attempt stale resume write");
    });
    let newer_write = std::thread::spawn(move || {
        barrier.wait();
        store.record_session(&newer).expect("write newer record");
        store
            .record_resume(&newer_resume)
            .expect("write newer resume");
        newer_written.send(()).expect("release older writer");
    });
    newer_write.join().expect("newer writer");
    older_write.join().expect("older writer");
}

#[tokio::test]
async fn report_native_id_ignores_reports_from_a_different_agent_base() {
    let store_path = temp_store_path("report-agent-mismatch");
    let agents_dir = temp_resumable_agents_dir("report-agent-mismatch");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(resumable_params())
        .await
        .expect("create claude session");

    let claude_report = registry
        .report_native_id(native_report!(&registry;
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "claude-native".to_owned(),
            transcript_path: None,
        ))
        .await;
    assert!(claude_report.recorded);

    let codex_report = registry
        .report_native_id(native_report!(&registry;
            session_id: created.id.clone(),
            agent: "codex".to_owned(),
            native_session_id: "codex-thread".to_owned(),
            transcript_path: None,
        ))
        .await;
    assert!(
        !codex_report.recorded,
        "a codex hook must not overwrite a claude session binding"
    );

    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(
        inspected.native_session_id.as_deref(),
        Some("claude-native")
    );

    let persisted = crate::store::Store::new(store_path.clone())
        .load_resume()
        .expect("load store");
    assert_eq!(persisted.len(), 1);
    assert_eq!(
        persisted[0].native_session_id.as_deref(),
        Some("claude-native")
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "the ordered-claim scenario keeps rejection ordering and rollback assertions together"
)]
async fn report_native_id_rejects_stale_expired_and_mismatched_claims() {
    let agents_dir = temp_resumable_agents_dir("report-ordered-claims");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(resumable_params())
        .await
        .expect("create session");
    let coordinates = native_report_params(
        &registry,
        created.id.clone(),
        "claude".to_owned(),
        "coordinate-only".to_owned(),
        None,
    )
    .await;
    let claim = |session_id: SessionId,
                 worker_instance_id: &str,
                 start_identity: ProcessStartIdentity,
                 sequence: u64,
                 expires_at: &str,
                 native_id: &str| {
        SessionReportNativeIdParams::new(
            session_id,
            worker_instance_id,
            "claude",
            coordinates.pid(),
            start_identity,
            ReportSequence::new(sequence),
            expires_at,
            native_id,
            None,
        )
        .expect("valid report shape")
    };
    let valid_expiry = native_report_expiry();

    let first = registry
        .report_native_id(claim(
            created.id.clone(),
            coordinates.worker_instance_id(),
            coordinates.pid_start_identity(),
            100,
            &valid_expiry,
            "native-current",
        ))
        .await;
    assert!(first.recorded);

    for rejected in [
        claim(
            created.id.clone(),
            coordinates.worker_instance_id(),
            coordinates.pid_start_identity(),
            100,
            &valid_expiry,
            "native-duplicate",
        ),
        claim(
            created.id.clone(),
            coordinates.worker_instance_id(),
            coordinates.pid_start_identity(),
            99,
            &valid_expiry,
            "native-lower",
        ),
        claim(
            created.id.clone(),
            "runtime-stale",
            coordinates.pid_start_identity(),
            101,
            &valid_expiry,
            "native-stale-runtime",
        ),
        claim(
            created.id.clone(),
            coordinates.worker_instance_id(),
            ProcessStartIdentity::new(coordinates.pid_start_identity().get() + 1),
            101,
            &valid_expiry,
            "native-reused-pid",
        ),
        claim(
            created.id.clone(),
            coordinates.worker_instance_id(),
            coordinates.pid_start_identity(),
            101,
            "2000-01-01T00:00:00Z",
            "native-expired",
        ),
        claim(
            SessionId("s-other".to_owned()),
            coordinates.worker_instance_id(),
            coordinates.pid_start_identity(),
            101,
            &valid_expiry,
            "native-wrong-session",
        ),
    ] {
        assert!(!registry.report_native_id(rejected).await.recorded);
    }

    let inspected = registry
        .inspect(&created.id)
        .await
        .expect("inspect session");
    assert_eq!(
        inspected.native_session_id.as_deref(),
        Some("native-current")
    );
    let _ = registry.stop(&created.id).await;
}

/// Create a temp `agents/` dir holding one profile file; return the dir path.
fn temp_agents_dir_with(tag: &str, name: &str, body: &str) -> PathBuf {
    let dir = temp_store_path(tag)
        .parent()
        .expect("store parent")
        .join("agents");
    std::fs::create_dir_all(&dir).expect("create agents dir");
    std::fs::write(dir.join(format!("{name}.toml")), body).expect("write profile");
    dir
}

fn write_agent_manifest(dir: &std::path::Path, name: &str, body: &str) {
    let manifests = dir.join("manifests");
    std::fs::create_dir_all(&manifests).expect("create manifests dir");
    std::fs::write(manifests.join(format!("{name}.toml")), body).expect("write manifest");
}

fn temp_resumable_agents_dir(tag: &str) -> PathBuf {
    temp_agents_dir_with(
        tag,
        "resumable",
        "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30\"]\n",
    )
}

/// Resumable agent whose first run stays alive until the returned gate is
/// written, and whose `--resume` run stays alive for the whole test.
///
/// The test decides when the first runtime ends (one line on the gate per
/// running agent), so the exit never races a fixed agent lifetime against
/// session creation and native-id capture. The gate handle must outlive the
/// agents: dropping it releases every waiting agent. Each run appends its argv
/// to `marker`.
#[cfg(unix)]
fn temp_agent_that_exits_then_resumes(tag: &str, marker: &std::path::Path) -> (PathBuf, fs::File) {
    let runtime = temp_dir(&format!("{tag}-runtime"));
    let script = runtime.join("resume-agent");
    let gate_path = runtime.join("exit.gate");
    let gate = hook_gate(&gate_path);
    write_executable(
        &script,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> {}\ncase \" $* \" in *\" --resume \"*) sleep 30 ;; *) read _ < '{}'; exit 0 ;; esac\n",
            marker.display(),
            gate_path.display(),
        ),
    );
    let agents_dir = temp_agents_dir_with(
        tag,
        "resumable",
        &format!(
            "base = \"claude\"\nprogram = \"{}\"\nargs = [\"--model\", \"sonnet\"]\n",
            script.display()
        ),
    );
    (agents_dir, gate)
}

#[cfg(unix)]
fn temp_hermes_that_exits_then_resumes(
    tag: &str,
    marker: &std::path::Path,
) -> (PathBuf, PathBuf, fs::File) {
    let runtime = temp_dir(&format!("{tag}-runtime"));
    let script = runtime.join("hermes");
    let gate = hook_gate(&runtime.join(HERMES_EXIT_GATE));
    write_hermes_resume_executable(&script, marker, "Hermes Agent v0.20.0");
    let agents = temp_agents_dir_with(
        tag,
        "hermes-test",
        &format!(
            "base = \"hermes\"\nprogram = \"{}\"\nargs = [\"chat\"]\n",
            script.display()
        ),
    );
    (agents, script, gate)
}

/// Name of the FIFO, beside the Hermes test executable, that releases the first
/// run's exit (see [`temp_agent_that_exits_then_resumes`]).
#[cfg(unix)]
const HERMES_EXIT_GATE: &str = "exit.gate";

/// Writes the Hermes test executable: `--version` reports `version_output`, the
/// first run waits for one line on the [`HERMES_EXIT_GATE`] FIFO beside it, and
/// `--resume` runs stay alive.
#[cfg(unix)]
fn write_hermes_resume_executable(
    script: &std::path::Path,
    marker: &std::path::Path,
    version_output: &str,
) {
    let gate = script
        .parent()
        .expect("script directory")
        .join(HERMES_EXIT_GATE);
    write_executable(
        script,
        &format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  printf '%s\\n' '{version_output}'\n  exit 0\nfi\nprintf '<%s>\\n' \"$@\" >> {}\ncase \" $* \" in *\" --resume \"*) sleep 30 ;; *) read _ < '{}'; exit 0 ;; esac\n",
            marker.display(),
            gate.display(),
        ),
    );
}

fn resumable_params() -> SessionNewParams {
    SessionNewParams {
        name: None,
        agent: "resumable".to_owned(),
        ..params()
    }
}

#[cfg(unix)]
#[tokio::test]
async fn fork_live_claude_session_mints_new_id_and_builds_fork_argv() {
    let dir = temp_dir("fork-claude-runtime");
    let script = dir.join("fork-agent");
    let marker = dir.join("argv.txt");
    write_executable(
        &script,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> {}\nsleep 30\n",
            marker.display()
        ),
    );
    let agents_dir = temp_agents_dir_with(
        "fork-claude",
        "forkable",
        &format!(
            "base = \"claude\"\nprogram = \"{}\"\nargs = [\"--model\", \"sonnet\"]\n",
            script.display()
        ),
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(SessionNewParams {
            agent: "forkable".to_owned(),
            cwd: Some(dir.clone()),
            ..params()
        })
        .await
        .expect("create forkable session");
    assert_eq!(created.state, SessionState::Running);
    let recorded = registry
        .report_native_id(native_report!(&registry;
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "native-live".to_owned(),
            transcript_path: None,
        ))
        .await;
    assert!(recorded.recorded);

    let forked = registry
        .fork(SessionForkParams {
            session_id: created.id.clone(),
            name: Some("forked review".to_owned()),
            cwd_mode: ForkCwdMode::Same,
            cols: 100,
            rows: 30,
            accept_profile_change: false,
        })
        .await
        .expect("fork live claude session");

    assert_ne!(forked.id, created.id, "fork must mint a fresh pohunek id");
    assert_eq!(forked.name.as_deref(), Some("forked review"));
    assert_eq!(forked.cwd, created.cwd);
    assert_eq!((forked.cols, forked.rows), (100, 30));
    assert_eq!(forked.native_session_id.as_deref(), Some("native-live"));

    let argv = wait_for_file_contains(&marker, "--fork-session").await;
    let lines = argv.lines().collect::<Vec<_>>();
    assert!(
        lines.windows(5).any(|window| {
            window
                == [
                    "--model",
                    "sonnet",
                    "--resume",
                    "native-live",
                    "--fork-session",
                ]
        }),
        "fork argv must preserve frozen args and append the Claude fork flag: {argv:?}"
    );

    let _ = registry.stop(&created.id).await;
    let _ = registry.stop(&forked.id).await;
}

#[tokio::test]
async fn fork_shell_session_reports_agent_fork_unsupported() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");

    let err = registry
        .fork(SessionForkParams {
            session_id: created.id.clone(),
            name: None,
            cwd_mode: ForkCwdMode::Same,
            cols: 80,
            rows: 24,
            accept_profile_change: false,
        })
        .await
        .expect_err("shell sessions cannot be forked");

    assert_eq!(err, protocol::ProtocolError::agent_fork_unsupported());
    assert_eq!(registry.list().await.len(), 1);
    let _ = registry.stop(&created.id).await;
}

#[cfg(unix)]
#[tokio::test]
async fn fork_codex_session_reports_agent_fork_unsupported() {
    let dir = temp_dir("fork-codex-runtime");
    let script = dir.join("codex-agent");
    write_executable(&script, "#!/bin/sh\nsleep 30\n");
    let agents_dir = temp_agents_dir_with(
        "fork-codex",
        "codex-fork",
        &format!("base = \"codex\"\nprogram = \"{}\"\n", script.display()),
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(SessionNewParams {
            agent: "codex-fork".to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect("create codex session");
    let err = registry
        .fork(SessionForkParams {
            session_id: created.id.clone(),
            name: None,
            cwd_mode: ForkCwdMode::Same,
            cols: 80,
            rows: 24,
            accept_profile_change: false,
        })
        .await
        .expect_err("codex fork is intentionally unsupported");

    assert_eq!(err, protocol::ProtocolError::agent_fork_unsupported());
    assert_eq!(
        registry.list().await.len(),
        1,
        "unsupported fork must fail before registering a logical child or worker"
    );
    let _ = registry.stop(&created.id).await;
}

#[cfg(unix)]
#[tokio::test]
async fn fork_hermes_session_is_rejected_before_child_side_effects() {
    let dir = temp_dir("fork-hermes-runtime");
    let script = dir.join("hermes");
    write_supported_hermes_executable(&script, "sleep 30\n");
    let agents_dir = temp_agents_dir_with(
        "fork-hermes",
        "hermes-test",
        &format!(
            "base = \"hermes\"\nprogram = \"{}\"\nargs = [\"chat\"]\n",
            script.display()
        ),
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(SessionNewParams {
            agent: "hermes-test".to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect("create Hermes session");

    let error = registry
        .fork(SessionForkParams {
            session_id: created.id.clone(),
            name: Some("must-not-exist".to_owned()),
            cwd_mode: ForkCwdMode::Same,
            cols: 80,
            rows: 24,
            accept_profile_change: false,
        })
        .await
        .expect_err("Hermes fork is unsupported");

    assert_eq!(error, protocol::ProtocolError::agent_fork_unsupported());
    assert_eq!(registry.list().await.len(), 1);
    assert!(registry
        .list()
        .await
        .iter()
        .all(|session| session.name.as_deref() != Some("must-not-exist")));
    let _ = registry.stop(&created.id).await;
}

#[cfg(unix)]
#[tokio::test]
async fn non_resumable_claude_profile_has_neither_resume_nor_fork() {
    let dir = temp_dir("non-resumable-claude");
    let script = dir.join("claude-agent");
    write_executable(&script, "#!/bin/sh\nsleep 30\n");
    let store_path = temp_store_path("non-resumable-claude");
    let agents_dir = temp_agents_dir_with(
        "non-resumable-claude",
        "no-recovery",
        &format!(
            "base = \"claude\"\nprogram = \"{}\"\n[resume]\nresumable = false\n",
            script.display()
        ),
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        agents_dir: Some(agents_dir),
        store_path: Some(store_path.clone()),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(SessionNewParams {
            agent: "no-recovery".to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect("create non-resumable Claude session");
    assert!(!created.capabilities.resume);
    assert!(!created.capabilities.fork);
    let worker_instance_id = created
        .runtime
        .as_ref()
        .and_then(|runtime| runtime.worker_instance_id.clone())
        .expect("live runtime id");

    let result = registry
        .report_native_id(native_report!(&registry;
            session_id: created.id.clone(),
            worker_instance_id: worker_instance_id,
            agent: "claude".to_owned(),
            pid: created.pid,
            pid_start_identity: 1,
            sequence: 1,
            expires_at: native_report_expiry(),
            native_session_id: "ignored-native".to_owned(),
            transcript_path: None,
        ))
        .await;
    assert!(
        !result.recorded,
        "no native recovery means no native id is recorded"
    );
    assert!(crate::store::Store::new(store_path)
        .load_resume()
        .expect("load resume bindings")
        .is_empty());
    let _ = registry.stop(&created.id).await;
}

#[cfg(unix)]
#[tokio::test]
async fn fork_disable_is_frozen_across_profile_removal() {
    let dir = temp_dir("fork-disable-frozen-runtime");
    let script = dir.join("claude-agent");
    write_executable(&script, "#!/bin/sh\nsleep 30\n");
    let agents_dir = temp_agents_dir_with(
        "fork-disable-frozen",
        "no-fork",
        &format!(
            "base = \"claude\"\nprogram = \"{}\"\n[resume]\nreference_kind = \"id\"\nargs = [\"--resume\", \"{{reference}}\"]\n",
            script.display()
        ),
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        agents_dir: Some(agents_dir.clone()),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(SessionNewParams {
            agent: "no-fork".to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect("create fork-disabled Claude session");
    std::fs::remove_file(agents_dir.join("no-fork.toml")).expect("remove profile after launch");

    let err = registry
        .fork(SessionForkParams {
            session_id: created.id.clone(),
            name: None,
            cwd_mode: ForkCwdMode::Same,
            cols: 80,
            rows: 24,
            accept_profile_change: false,
        })
        .await
        .expect_err("frozen fork disable must survive profile removal");

    assert_eq!(err.code, "agent_fork_unsupported");
    assert_eq!(registry.list().await.len(), 1);
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn report_native_id_path_profile_stores_path_and_ignores_wire_agent() {
    // The load-bearing C.3 fix: a `reference_kind = "path"` profile must store the
    // native reference into `native_session_path` (clearing `native_session_id`),
    // chosen by the FROZEN snapshot — never by the wire `agent` literal, which
    // the SessionStart hook bakes to a base-kind name carrying no profile id.
    let store_path = temp_store_path("path-profile");
    let agents_dir = temp_agents_dir_with(
            "path-profile",
            "pathy",
            "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30\"]\n[resume]\nreference_kind = \"path\"\nargs = [\"--resume\", \"{reference}\"]\n",
        );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir),
        socket_path: Some(PathBuf::from("/run/pohunek/d.sock")),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(SessionNewParams {
            agent: "pathy".to_owned(),
            ..params()
        })
        .await
        .expect("create path-profile session");

    let result = registry
        .report_native_id(native_report!(&registry;
            session_id: created.id.clone(),
            // Wire agent is a base-kind literal (what the hook reports); ignored
            // for ref-kind selection.
            agent: "claude".to_owned(),
            native_session_id: "opaque-native-id".to_owned(),
            transcript_path: Some("/home/u/.claude/t.jsonl".to_owned()),
        ))
        .await;
    assert!(result.recorded);

    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(
        inspected.native_session_path.as_deref(),
        Some("/home/u/.claude/t.jsonl"),
        "a path-kind profile stores into native_session_path"
    );
    assert_eq!(
        inspected.native_session_id, None,
        "the id field is left empty for a path-kind session"
    );

    let persisted = crate::store::Store::new(store_path.clone())
        .load_resume()
        .expect("load store");
    assert_eq!(persisted.len(), 1);
    assert_eq!(
        persisted[0].native_session_path.as_deref(),
        Some("/home/u/.claude/t.jsonl")
    );
    assert_eq!(persisted[0].native_session_id, None);
    assert_eq!(persisted[0].reference_kind(), Some(SessionRefKind::Path));
    assert!(persisted[0].resumable());
    assert!(!persisted[0].forkable());
    assert_eq!(persisted[0].program, "/bin/sh");
    assert!(persisted[0].resumable());

    registry
        .resize(&created.id, 120, 40)
        .await
        .expect("resize path-profile session");
    let resized = crate::store::Store::new(store_path)
        .load_resume()
        .expect("load resized binding");
    assert_eq!(
        (resized[0].cols, resized[0].rows),
        (120, 40),
        "path-kind resume binding must refresh dimensions after resize"
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn non_resumable_profile_ignores_native_id_reports() {
    let store_path = temp_store_path("noresume-profile");
    let agents_dir = temp_agents_dir_with(
            "noresume-profile",
            "noresume",
            "base = \"codex\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30\"]\n[resume]\nresumable = false\n",
        );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir),
        socket_path: Some(PathBuf::from("/run/pohunek/d.sock")),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(SessionNewParams {
            agent: "noresume".to_owned(),
            ..params()
        })
        .await
        .expect("create non-resumable profile session");

    let result = registry
        .report_native_id(native_report!(&registry;
            session_id: created.id.clone(),
            agent: "codex".to_owned(),
            native_session_id: "native-ignored".to_owned(),
            transcript_path: None,
        ))
        .await;
    assert!(
        !result.recorded,
        "non-resumable profile must reject native-id reports fail-closed"
    );
    assert!(
        crate::store::Store::new(store_path)
            .load_resume()
            .expect("load")
            .is_empty(),
        "non-resumable profile must not persist a resume binding"
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn non_resumable_profile_binding_reports_agent_not_resumable() {
    let registry = SessionRegistry::default();
    let binding = crate::store::ResumeBinding {
        session_id: "s-noresume".to_owned(),
        name: None,
        agent: "noresume".to_owned(),
        agent_base: RuntimeRef::codex(),
        cwd: temp_dir("noresume-binding-cwd"),
        cols: 80,
        rows: 24,
        native_session_id: Some("native-ignored".to_owned()),
        native_session_path: None,
        project_id: None,
        is_linked_worktree: None,
        metadata: BTreeMap::new(),
        program: "/bin/sh".to_owned(),
        args: Vec::new(),
        input_rules: crate::store::StoredInputRules::default(),
        native_launch: None,
        launch_binding: crate::agent::host::LaunchPin::Unpinned,
        native_reference_provenance: crate::agent::NativeReferenceProvenance::default(),
        profile_revision: None,
        native_launch_unresolved: false,
    };

    let err = registry
        .resume_binding(binding)
        .await
        .expect_err("non-resumable binding must fail");
    assert_eq!(err.code, "agent_not_resumable");
}

#[tokio::test]
async fn resume_binding_never_persists_profile_env_secrets() {
    // C.4 no-secrets invariant: a profile's `[env]` (which may hold secrets) is
    // never written to the store. The serialized resume line must contain none
    // of the env keys OR values — env is re-resolved by agent name at resume.
    let store_path = temp_store_path("env-secret");
    let agents_dir = temp_agents_dir_with(
            "env-secret",
            "withenv",
            "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30\"]\n[env]\nSECRET_TOKEN = \"supersecretvalue\"\n",
        );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir),
        socket_path: Some(PathBuf::from("/run/pohunek/d.sock")),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(SessionNewParams {
            agent: "withenv".to_owned(),
            ..params()
        })
        .await
        .expect("create env-profile session");
    let result = registry
        .report_native_id(native_report!(&registry;
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "native-xyz".to_owned(),
            transcript_path: None,
        ))
        .await;
    assert!(result.recorded);

    let raw = std::fs::read_to_string(&store_path).expect("read store file");
    assert!(
        !raw.contains("SECRET_TOKEN") && !raw.contains("supersecretvalue"),
        "profile env (key or value) must never reach the store: {raw}"
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn stopping_a_session_drops_its_resume_binding() {
    let store_path = temp_store_path("drop-on-stop");
    let agents_dir = temp_resumable_agents_dir("drop-on-stop");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(resumable_params())
        .await
        .expect("create session");
    let recorded = registry
        .report_native_id(native_report!(&registry;
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "native-stop".to_owned(),
            transcript_path: None,
        ))
        .await;
    assert!(recorded.recorded);
    assert_eq!(
        crate::store::Store::new(store_path.clone())
            .load_resume()
            .expect("load")
            .len(),
        1
    );

    // Stopping the session must drop the binding so a restart does not
    // resurrect a session the user ended.
    let stopped = registry.stop(&created.id).await.expect("stop");
    assert!(stopped.stopped);
    assert!(
        crate::store::Store::new(store_path)
            .load_resume()
            .expect("load")
            .is_empty(),
        "stopped session must not leave a resume binding"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn legacy_harness_exit_during_daemon_shutdown_keeps_recovery_binding() {
    let store_path = temp_store_path("shutdown-keeps-binding");
    let agents_dir = temp_resumable_agents_dir("shutdown-keeps-binding");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(resumable_params())
        .await
        .expect("create session");
    let recorded = registry
        .report_native_id(native_report!(&registry;
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "native-shutdown".to_owned(),
            transcript_path: None,
        ))
        .await;
    assert!(recorded.recorded, "native id captured");
    assert_eq!(
        crate::store::Store::new(store_path.clone())
            .load_resume()
            .expect("load before shutdown")
            .len(),
        1,
        "precondition: captured session has one resume binding"
    );

    registry.begin_daemon_shutdown();
    let _ = registry
        .record_exit(
            &created.id,
            RuntimeExit {
                exit_code: None,
                success: false,
            },
            false,
            None,
            None,
        )
        .await;

    let persisted = crate::store::Store::new(store_path)
        .load_resume()
        .expect("load after shutdown exit");
    let _ = registry.stop(&created.id).await;
    terminate_pid(created.pid);

    assert_eq!(
        persisted.len(),
        1,
        "a synthetic harness exit during daemon shutdown must keep recovery metadata"
    );
    assert_eq!(persisted[0].session_id, created.id.0);
    assert_eq!(
        persisted[0].native_session_id.as_deref(),
        Some("native-shutdown")
    );
}

#[cfg(unix)]
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "the recovery scenario verifies one ordered lifecycle across persistence and events"
)]
async fn explicit_native_recovery_from_lost_preserves_identity_emits_event_and_is_idempotent() {
    let store_path = temp_store_path("manual-resume");
    let marker = temp_dir("manual-resume-marker").join("argv.txt");
    let (agents_dir, mut exit_gate) = temp_agent_that_exits_then_resumes("manual-resume", &marker);
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path),
        agents_dir: Some(agents_dir),
        socket_path: Some(PathBuf::from("/run/pohunek/d.sock")),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(resumable_params())
        .await
        .expect("create session");
    let recorded = registry
        .report_native_id(native_report!(&registry;
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "native-manual".to_owned(),
            transcript_path: None,
        ))
        .await;
    assert!(recorded.recorded, "native id captured");
    exit_gate
        .write_all(b"go\n")
        .expect("release the agent exit gate");

    let done = registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("session exits");
    assert_eq!(done.state, SessionState::Done);
    let original_created_at = done.created_at.clone();
    let mut sessions = registry.inner.sessions.lock().await;
    let entry = sessions.get_mut(&created.id).expect("terminal entry");
    entry.info.runtime = Some(SessionRuntime {
        state: RuntimeState::Lost,
        runtime_generation: protocol::RuntimeGeneration::new(1),
        worker_id: Some("worker-before-recovery".to_owned()),
        worker_instance_id: Some("runtime-before-recovery".to_owned()),
        started_at: Some(original_created_at.clone()),
        last_connected_at: None,
        loss_reason: Some("test_runtime_lost".to_owned()),
    });
    entry.runtime = super::RuntimeHandle::Unavailable(RuntimeState::Lost);
    let lost_job = entry.job.clone().expect("created session records its job");
    drop(sessions);
    // A lost runtime has no worker left: retire the retained terminal worker
    // so the supervisor evidence matches the injected `Lost` state.
    registry
        .lifecycle()
        .expect("test registry supervises workers")
        .retire(&lost_job)
        .await
        .expect("retire the lost generation");
    let mut events = registry.subscribe();

    let resumed = registry
        .resume(&created.id)
        .await
        .expect("resume terminal session");
    assert_eq!(resumed.id, created.id);
    assert_eq!(resumed.created_at, original_created_at);
    assert_eq!(resumed.state, SessionState::Running);
    assert_eq!(resumed.native_session_id.as_deref(), Some("native-manual"));
    assert_eq!(
        resumed
            .runtime
            .as_ref()
            .expect("resumed runtime")
            .runtime_generation,
        RuntimeGeneration::new(2)
    );

    let argv = wait_for_file_contains(&marker, "native-manual").await;
    assert!(
        argv.contains("--resume") && argv.contains("native-manual"),
        "resume argv must target the captured native id: {argv:?}"
    );

    let event = guard("the session_native_recovered event", async {
        loop {
            let event = events.recv().await.expect("receive recovery event");
            if event.event() == protocol::event::SESSION_NATIVE_RECOVERED {
                break event;
            }
        }
    })
    .await;
    let recovered_event: SessionNativeRecoveredEvent =
        serde_json::from_value(event.payload().clone()).expect("recovery event payload");
    assert_eq!(recovered_event.session.id, created.id);
    assert_eq!(
        recovered_event.previous_worker_instance_id.as_deref(),
        Some("runtime-before-recovery")
    );
    // The durable-worker backend always mints a fresh runtime generation on
    // `initialize`, including for explicit native recovery, so the recovered
    // event must carry a *new* id distinct from the replaced generation.
    assert_ne!(
        recovered_event.worker_instance_id.as_deref(),
        Some("runtime-before-recovery"),
        "native recovery must mint a new worker runtime, not reuse the previous one"
    );
    assert!(
        recovered_event.worker_instance_id.is_some(),
        "native recovery must mint a fresh durable-worker runtime id"
    );

    let repeated = registry
        .resume(&created.id)
        .await
        .expect_err("live recovered session is not recoverable again");
    assert_eq!(repeated.code, "session_runtime_not_recoverable");
    assert_eq!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect after repeated recovery")
            .pid,
        resumed.pid
    );

    let _ = registry.stop(&created.id).await;
}

#[cfg(unix)]
#[tokio::test]
async fn hermes_resume_reuses_logical_session_with_exact_argv_and_new_generation() {
    let marker = temp_dir("hermes-resume-marker").join("argv.txt");
    let (agents_dir, _script, mut exit_gate) =
        temp_hermes_that_exits_then_resumes("hermes-resume", &marker);
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(temp_store_path("hermes-resume")),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(SessionNewParams {
            agent: "hermes-test".to_owned(),
            ..params()
        })
        .await
        .expect("create Hermes session");
    let native_reference = "native id with spaces + symbols";
    assert!(
        registry
            .report_native_id(native_report!(&registry;
                session_id: created.id.clone(),
                agent: "hermes".to_owned(),
                native_session_id: native_reference.to_owned(),
                transcript_path: None,
            ))
            .await
            .recorded
    );
    exit_gate
        .write_all(b"go\n")
        .expect("release the Hermes exit gate");
    let terminal = registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("fresh Hermes process exits");
    let initial_generation = terminal
        .runtime
        .as_ref()
        .expect("terminal runtime")
        .runtime_generation;

    let resumed = registry
        .resume(&created.id)
        .await
        .expect("resume Hermes session");

    assert_eq!(resumed.id, created.id);
    assert_eq!(resumed.created_at, created.created_at);
    assert_eq!(
        resumed
            .runtime
            .as_ref()
            .expect("resumed runtime")
            .runtime_generation,
        RuntimeGeneration::new(initial_generation.get() + 1)
    );
    let argv = wait_for_file_contains(&marker, native_reference).await;
    let lines = argv.lines().collect::<Vec<_>>();
    assert!(
        lines
            .windows(3)
            .any(|window| window == ["<chat>", "<--resume>", "<native id with spaces + symbols>"]),
        "Hermes resume argv must be exact and keep the reference in one element: {lines:?}"
    );
    assert!(!argv.contains("--continue"));
    assert!(!argv.contains("--pass-session-id"));
    let _ = registry.stop(&resumed.id).await;
}

#[cfg(unix)]
#[tokio::test]
async fn incompatible_hermes_resume_has_no_runtime_or_store_side_effects() {
    let marker = temp_dir("hermes-policy-resume-marker").join("argv.txt");
    let (agents_dir, script, mut exit_gate) =
        temp_hermes_that_exits_then_resumes("hermes-policy-resume", &marker);
    let store_path = temp_store_path("hermes-policy-resume");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(SessionNewParams {
            agent: "hermes-test".to_owned(),
            ..params()
        })
        .await
        .expect("create supported Hermes session");
    assert!(
        registry
            .report_native_id(native_report!(&registry;
                session_id: created.id.clone(),
                agent: "hermes".to_owned(),
                native_session_id: "native-policy-test".to_owned(),
                transcript_path: None,
            ))
            .await
            .recorded
    );
    exit_gate
        .write_all(b"go\n")
        .expect("release the Hermes exit gate");
    let terminal = registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("fresh Hermes process exits");
    wait_for_resume_binding_removed(&registry, &created.id).await;
    let marker_before = fs::read_to_string(&marker).expect("fresh launch marker");
    let store_before = fs::read(&store_path).expect("durable session store");

    for version_output in ["Hermes Agent v0.21.0", "unexpected provider output"] {
        write_hermes_resume_executable(&script, &marker, version_output);
        let error = registry
            .resume(&created.id)
            .await
            .expect_err("incompatible Hermes runtime must not resume");
        assert_eq!(error.code, "agent_runtime_unsupported");
        assert!(!error.msg.contains(version_output));
        assert_eq!(
            fs::read_to_string(&marker).expect("launch marker"),
            marker_before
        );
        assert_eq!(fs::read(&store_path).expect("session store"), store_before);
        assert_eq!(registry.list().await.len(), 1);
        let after = registry
            .inspect(&created.id)
            .await
            .expect("inspect terminal");
        assert_eq!(after.state, terminal.state);
        assert_eq!(after.pid, terminal.pid);
        assert_eq!(after.runtime, terminal.runtime);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn incompatible_hermes_resume_binding_fails_before_recovery_side_effects() {
    let dir = temp_dir("hermes-policy-resume-binding");
    let script = dir.join("hermes");
    let missing = dir.join("missing-hermes");
    let marker = dir.join("argv.txt");
    let store_path = dir.join("sessions.jsonl");
    let sentinel = b"store must not be read or rewritten\n";
    fs::write(&store_path, sentinel).expect("seed untouched store sentinel");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        store_path: Some(store_path.clone()),
        ..SessionRegistryConfig::default()
    });
    let binding = crate::store::ResumeBinding {
        session_id: "s-hermes-policy".to_owned(),
        name: None,
        agent: "hermes".to_owned(),
        agent_base: RuntimeRef::hermes(),
        cwd: dir.clone(),
        cols: 80,
        rows: 24,
        native_session_id: Some("native-policy-test".to_owned()),
        native_session_path: None,
        project_id: None,
        is_linked_worktree: None,
        metadata: BTreeMap::new(),
        program: script.display().to_string(),
        args: vec!["chat".to_owned()],
        input_rules: crate::store::StoredInputRules::default(),
        native_launch: Some(test_native_launch(SessionRefKind::Id, false)),
        launch_binding: crate::agent::host::LaunchPin::Unpinned,
        native_reference_provenance: crate::agent::NativeReferenceProvenance::default(),
        profile_revision: None,
        native_launch_unresolved: false,
    };

    for (program, version_output) in [
        (Some(script.clone()), "Hermes Agent v0.21.0"),
        (Some(script.clone()), "unexpected provider output"),
        (None, "missing"),
    ] {
        let mut candidate = binding.clone();
        if let Some(program) = program {
            write_hermes_resume_executable(&program, &marker, version_output);
            candidate.program = program.display().to_string();
        } else {
            candidate.program = missing.display().to_string();
        }

        let error = registry
            .resume_binding(candidate)
            .await
            .expect_err("incompatible Hermes binding must not enter recovery");
        assert_eq!(error.code, "agent_runtime_unsupported");
        assert_eq!(
            error.msg,
            "the selected agent runtime is unavailable or incompatible with this daemon"
        );
        assert!(error.recover.is_none());
        assert_eq!(fs::read(&store_path).expect("store sentinel"), sentinel);
        assert!(!marker.exists(), "resume process must not be launched");
        assert!(registry.list().await.is_empty());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn hermes_resume_without_native_reference_fails_before_relaunch() {
    let marker = temp_dir("hermes-no-reference-marker").join("argv.txt");
    let (agents_dir, _script, mut exit_gate) =
        temp_hermes_that_exits_then_resumes("hermes-no-reference", &marker);
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(SessionNewParams {
            agent: "hermes-test".to_owned(),
            ..params()
        })
        .await
        .expect("create Hermes session");
    exit_gate
        .write_all(b"go\n")
        .expect("release the Hermes exit gate");
    registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("fresh Hermes process exits");
    let before = fs::read_to_string(&marker).expect("fresh argv marker");

    let error = registry
        .resume(&created.id)
        .await
        .expect_err("resume requires an exact native reference");

    assert_eq!(error.code, "not_resumable");
    assert_eq!(
        fs::read_to_string(&marker).expect("argv marker after rejection"),
        before,
        "missing-reference rejection must not launch another process"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_native_recovery_accepts_terminal_runtime() {
    let marker = temp_dir("terminal-recovery-marker").join("argv.txt");
    let (agents_dir, mut gate) = temp_agent_that_exits_then_resumes("terminal-recovery", &marker);
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        agents_dir: Some(agents_dir),
        socket_path: Some(PathBuf::from("/run/pohunek/d.sock")),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(resumable_params())
        .await
        .expect("create session");
    assert!(
        registry
            .report_native_id(native_report!(&registry;
                session_id: created.id.clone(),
                agent: "claude".to_owned(),
                native_session_id: "native-terminal".to_owned(),
                transcript_path: None,
            ))
            .await
            .recorded
    );
    // The agent exits only now, after its native id was recorded.
    gate.write_all(b"go\n")
        .expect("release the agent exit gate");
    let terminal = registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("session exits");
    assert_eq!(terminal.state, SessionState::Done);

    let recovered = registry
        .resume(&created.id)
        .await
        .expect("terminal runtime is eligible for native recovery");
    assert_eq!(recovered.id, created.id);
    assert_eq!(recovered.created_at, created.created_at);
    assert_eq!(recovered.state, SessionState::Running);
    assert!(wait_for_file_contains(&marker, "native-terminal")
        .await
        .contains("--resume"));

    let _ = registry.stop(&created.id).await;
}

#[cfg(unix)]
#[tokio::test]
async fn recovery_keeps_a_hostile_native_id_as_one_argv_element() {
    let marker = temp_dir("hostile-id-marker").join("argv.txt");
    let (agents_dir, mut gate) = temp_agent_that_exits_then_resumes("hostile-id", &marker);
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        agents_dir: Some(agents_dir),
        socket_path: Some(PathBuf::from("/run/pohunek/d.sock")),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(resumable_params())
        .await
        .expect("create session");
    let hostile = "native id;$(touch pwned)|`x` 'q' \"z\" *";
    assert!(
        registry
            .report_native_id(native_report!(&registry;
                session_id: created.id.clone(),
                agent: "claude".to_owned(),
                native_session_id: hostile.to_owned(),
                transcript_path: None,
            ))
            .await
            .recorded
    );
    gate.write_all(b"go\n")
        .expect("release the agent exit gate");
    registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("session exits");

    registry.resume(&created.id).await.expect("native recovery");
    let argv = wait_for_file_contains(&marker, "--resume").await;
    assert_eq!(
        argv.lines().collect::<Vec<_>>(),
        ["--model", "sonnet", "--model", "sonnet", "--resume", hostile],
        "the launch args stay frozen and the reference is one argv element"
    );

    let _ = registry.stop(&created.id).await;
}

#[cfg(unix)]
#[tokio::test]
async fn pi_shaped_profile_resumes_and_forks_through_the_frozen_spec() {
    let dir = temp_dir("pi-shaped-session");
    let marker = dir.join("argv.txt");
    let script = dir.join("pi-like");
    let gate_path = dir.join("exit.gate");
    let mut gate = hook_gate(&gate_path);
    write_executable(
        &script,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\ncase \"$1\" in --session|--fork) sleep 30 ;; *) read _ < '{}'; exit 0 ;; esac\n",
            marker.display(),
            gate_path.display(),
        ),
    );
    let agents_dir = temp_agents_dir_with(
        "pi-shaped-session",
        "pi-like",
        &format!(
            "base = \"claude\"\nprogram = \"{}\"\n[resume]\nreference_kind = \"path\"\nargs = [\"--session\", \"{{reference}}\"]\nfork_args = [\"--fork\", \"{{reference}}\"]\n",
            script.display()
        ),
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        agents_dir: Some(agents_dir),
        socket_path: Some(PathBuf::from("/run/pohunek/d.sock")),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(SessionNewParams {
            agent: "pi-like".to_owned(),
            cwd: Some(dir.clone()),
            ..params()
        })
        .await
        .expect("create Pi-shaped session");
    assert!(created.capabilities.resume && created.capabilities.fork);
    let reference = "/work/with space/$(touch pwned);|.jsonl";
    assert!(
        registry
            .report_native_id(native_report!(&registry;
                session_id: created.id.clone(),
                agent: "claude".to_owned(),
                native_session_id: "ignored-for-path-kind".to_owned(),
                transcript_path: Some(reference.to_owned()),
            ))
            .await
            .recorded
    );
    gate.write_all(b"go\n").expect("release the exit gate");
    registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("session exits");

    registry.resume(&created.id).await.expect("native recovery");
    let argv = wait_for_file_contains(&marker, "--session").await;
    // The first launch passes no arguments and prints one empty line.
    assert_eq!(
        argv.lines().skip(1).collect::<Vec<_>>(),
        ["--session", reference]
    );

    let forked = registry
        .fork(SessionForkParams {
            session_id: created.id.clone(),
            name: None,
            cwd_mode: ForkCwdMode::Same,
            cols: 80,
            rows: 24,
            accept_profile_change: false,
        })
        .await
        .expect("native fork");
    let argv = wait_for_file_contains(&marker, "--fork").await;
    assert_eq!(
        argv.lines().skip(1).collect::<Vec<_>>(),
        ["--session", reference, "--fork", reference]
    );

    let _ = registry.stop(&forked.id).await;
    let _ = registry.stop(&created.id).await;
}

#[cfg(unix)]
#[tokio::test]
async fn malformed_resume_template_fails_before_any_worker_or_record_exists() {
    let dir = temp_dir("malformed-template-session");
    let store_path = temp_store_path("malformed-template-session");
    let runtime_root = temp_dir("malformed-template-runtime");
    let agents_dir = temp_agents_dir_with(
        "malformed-template-session",
        "broken",
        "base = \"claude\"\n[resume]\nreference_kind = \"id\"\nargs = [\"--resume={reference}\"]\n",
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        agents_dir: Some(agents_dir),
        store_path: Some(store_path.clone()),
        worker_runtime_root: Some(runtime_root.clone()),
        ..SessionRegistryConfig::default()
    });

    let error = registry
        .create(SessionNewParams {
            agent: "broken".to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect_err("a malformed template must reject the launch");

    assert_eq!(error.code, "invalid_profile");
    assert!(error.msg.contains("'broken'") && error.msg.contains("resume.args"));
    assert!(registry.list().await.is_empty());
    assert!(
        !store_path.exists()
            || crate::store::Store::new(store_path)
                .load_resume()
                .expect("load resume bindings")
                .is_empty()
    );
    assert_eq!(
        fs::read_dir(&runtime_root).expect("runtime root").count(),
        0,
        "no worker socket or runtime state may exist"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_native_recovery_rejects_nonterminal_runtime_states() {
    let agents_dir = temp_resumable_agents_dir("recovery-preconditions");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(resumable_params())
        .await
        .expect("create resumable session");

    for state in [
        RuntimeState::Starting,
        RuntimeState::Live,
        RuntimeState::Reconnecting,
        RuntimeState::Conflict,
        RuntimeState::Incompatible,
    ] {
        let mut sessions = registry.inner.sessions.lock().await;
        let entry = sessions.get_mut(&created.id).expect("live entry");
        entry.info.runtime = Some(SessionRuntime {
            state,
            runtime_generation: protocol::RuntimeGeneration::new(1),
            worker_id: Some("worker-live".to_owned()),
            worker_instance_id: Some("runtime-live".to_owned()),
            started_at: None,
            last_connected_at: None,
            loss_reason: None,
        });
        drop(sessions);
        let error = registry
            .resume(&created.id)
            .await
            .expect_err("nonterminal runtime must reject native recovery");
        assert_eq!(
            error.code, "session_runtime_not_recoverable",
            "unexpected error for {state:?}"
        );
    }

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn native_recovered_event_carries_previous_and_new_worker_instance_ids() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let mut session = registry.create(params()).await.expect("create session");
    session.runtime = Some(SessionRuntime {
        state: RuntimeState::Live,
        runtime_generation: protocol::RuntimeGeneration::new(1),
        worker_id: Some("worker-new".to_owned()),
        worker_instance_id: Some("runtime-new".to_owned()),
        started_at: Some(session.created_at.clone()),
        last_connected_at: Some(session.updated_at.clone()),
        loss_reason: None,
    });
    let mut events = registry.subscribe();

    registry.emit_native_recovered(&session, Some("runtime-old".to_owned()));

    let event = events.recv().await.expect("native recovery event");
    assert_eq!(event.event(), protocol::event::SESSION_NATIVE_RECOVERED);
    let payload: SessionNativeRecoveredEvent =
        serde_json::from_value(event.payload().clone()).expect("recovery payload");
    assert_eq!(payload.session.id, session.id);
    assert_eq!(
        payload.previous_worker_instance_id.as_deref(),
        Some("runtime-old")
    );
    assert_eq!(payload.worker_instance_id.as_deref(), Some("runtime-new"));

    let _ = registry.stop(&session.id).await;
}

#[tokio::test]
async fn resize_after_capture_updates_persisted_binding() {
    let store_path = temp_store_path("resize-binding");
    let agents_dir = temp_resumable_agents_dir("resize-binding");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(resumable_params())
        .await
        .expect("create session");
    // Capture a native id so a resume binding exists at the launch size.
    let recorded = registry
        .report_native_id(native_report!(&registry;
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "native-resize".to_owned(),
            transcript_path: None,
        ))
        .await;
    assert!(recorded.recorded);
    let before = crate::store::Store::new(store_path.clone())
        .load_resume()
        .expect("load before");
    assert_eq!(before.len(), 1);
    assert_eq!((before[0].cols, before[0].rows), (80, 24));

    // Resizing the live session must refresh the persisted dimensions so a
    // restart resumes at the new size, not the stale capture-time size.
    registry
        .resize(&created.id, 132, 50)
        .await
        .expect("resize session");

    let after = crate::store::Store::new(store_path)
        .load_resume()
        .expect("load after");
    assert_eq!(
        after.len(),
        1,
        "resize must upsert, not duplicate: {after:?}"
    );
    assert_eq!(after[0].session_id, created.id.0);
    assert_eq!(after[0].native_session_id.as_deref(), Some("native-resize"));
    assert_eq!(
        (after[0].cols, after[0].rows),
        (132, 50),
        "persisted binding must carry the post-resize dimensions"
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn set_metadata_after_capture_updates_persisted_binding() {
    let store_path = temp_store_path("metadata-binding");
    let agents_dir = temp_resumable_agents_dir("metadata-binding");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(SessionNewParams {
            metadata: metadata(&[("owner", "cli"), ("ticket", "old")]),
            ..resumable_params()
        })
        .await
        .expect("create session");
    let recorded = registry
        .report_native_id(native_report!(&registry;
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "native-metadata".to_owned(),
            transcript_path: None,
        ))
        .await;
    assert!(recorded.recorded);

    let expected = metadata(&[("owner", "daemon"), ("reviewer", "qa"), ("ticket", "old")]);
    registry
        .set_metadata(
            &created.id,
            metadata_patch(&[("owner", Some("daemon")), ("reviewer", Some("qa"))]),
        )
        .await
        .expect("set metadata after capture");

    let persisted = crate::store::Store::new(store_path)
        .load_resume()
        .expect("load binding");
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].session_id, created.id.0);
    assert_eq!(persisted[0].metadata, expected);

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn resume_binding_restores_metadata_from_store() {
    let store_path = temp_store_path("resume-metadata");
    let expected = metadata(&[("owner", "daemon"), ("ticket", "DMD-1356")]);
    let store = crate::store::Store::new(store_path.clone());
    store
        .record_resume(&crate::store::ResumeBinding {
            session_id: "s-42".to_owned(),
            name: None,
            agent: "claude".to_owned(),
            agent_base: RuntimeRef::claude(),
            cwd: temp_dir("resume-metadata-cwd"),
            cols: 80,
            rows: 24,
            native_session_id: Some("native-metadata".to_owned()),
            native_session_path: None,
            project_id: None,
            is_linked_worktree: None,
            metadata: expected.clone(),
            program: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), "sleep 30".to_owned()],
            input_rules: crate::store::StoredInputRules::default(),
            native_launch: Some(test_native_launch(SessionRefKind::Id, false)),
            launch_binding: crate::agent::host::LaunchPin::Unpinned,
            native_reference_provenance: crate::agent::NativeReferenceProvenance::default(),
            profile_revision: None,
            native_launch_unresolved: false,
        })
        .expect("seed resume binding");
    let binding = crate::store::Store::new(store_path.clone())
        .load_resume()
        .expect("load resume binding")
        .into_iter()
        .next()
        .expect("one binding");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path),
        ..SessionRegistryConfig::default()
    });

    let resumed = registry
        .resume_binding(binding)
        .await
        .expect("resume binding");

    assert_eq!(resumed.metadata, expected);
    assert_eq!(
        registry
            .inspect(&resumed.id)
            .await
            .expect("inspect resumed")
            .metadata,
        expected
    );

    let _ = registry.stop(&resumed.id).await;
}

#[tokio::test]
async fn resize_without_captured_native_id_persists_no_binding() {
    let store_path = temp_store_path("resize-no-binding");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        ..SessionRegistryConfig::default()
    });

    let created = registry.create(params()).await.expect("create session");
    // No native id captured yet: resizing must not fabricate an unusable
    // recovery binding.
    registry
        .resize(&created.id, 100, 30)
        .await
        .expect("resize session");

    assert!(
        crate::store::Store::new(store_path)
            .load_resume()
            .expect("load")
            .is_empty(),
        "resize without a native id must not create a resume binding"
    );

    let _ = registry.stop(&created.id).await;
}

#[cfg(unix)]
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario: create, resize, edit, refuse, accept"
)]
async fn resume_after_profile_edit_and_resize_uses_original_snapshot() {
    let store_path = temp_store_path("resume-edit-resize");
    let dir = temp_dir("resume-edit-resize-runtime");
    let script_v1 = dir.join("agent-v1");
    let script_v2 = dir.join("agent-v2");
    let marker_v1 = dir.join("v1-argv.txt");
    let marker_v2 = dir.join("v2-argv.txt");
    write_resume_agent_script(&script_v1, &marker_v1);
    write_resume_agent_script(&script_v2, &marker_v2);
    let agents_dir = temp_agents_dir_with(
        "resume-edit-resize",
        "editable",
        &format!(
            "base = \"claude\"\nprogram = \"{}\"\nargs = [\"--model\", \"sonnet\"]\n",
            script_v1.display()
        ),
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir.clone()),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(SessionNewParams {
            agent: "editable".to_owned(),
            name: None,
            cwd: Some(dir.clone()),
            ..params()
        })
        .await
        .expect("create editable profile session");
    let recorded = registry
        .report_native_id(native_report!(&registry;
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "native-edit-resize".to_owned(),
            transcript_path: None,
        ))
        .await;
    assert!(recorded.recorded);
    registry
        .resize(&created.id, 123, 55)
        .await
        .expect("resize captured session");
    let binding = crate::store::Store::new(store_path.clone())
        .load_resume()
        .expect("load resized binding")
        .into_iter()
        .next()
        .expect("resume binding exists");
    assert_eq!((binding.cols, binding.rows), (123, 55));

    registry
        .stop(&created.id)
        .await
        .expect("stop original session after snapshot capture");
    fs::write(&marker_v1, "").expect("clear v1 marker");
    fs::write(
        agents_dir.join("editable.toml"),
        format!(
            "base = \"claude\"\nprogram = \"{}\"\nargs = [\"--model\", \"opus\"]\n",
            script_v2.display()
        ),
    )
    .expect("edit profile");

    let restarted = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });
    let refused = restarted
        .resume_binding(binding.clone())
        .await
        .expect_err("an edited profile is not relaunched without the owner's decision");
    assert_eq!(refused.code, "agent_profile_changed");
    assert_eq!(
        fs::read_to_string(&marker_v1).expect("read v1 marker"),
        "",
        "a refused resume launches nothing"
    );

    let resumed = restarted
        .resume_binding_with(binding, super::ProfileChange::Accept)
        .await
        .expect("resume from frozen binding once the owner accepted the edit");

    assert_eq!((resumed.cols, resumed.rows), (123, 55));
    let argv = wait_for_file_contains(&marker_v1, "native-edit-resize").await;
    assert_eq!(
        argv.lines().collect::<Vec<_>>(),
        vec!["--model", "sonnet", "--resume", "native-edit-resize"],
        "resume must use launch-time program/args, not the edited profile"
    );
    assert!(
        !marker_v2.exists()
            || fs::read_to_string(&marker_v2)
                .unwrap_or_default()
                .is_empty(),
        "edited profile program must not run during resume"
    );

    let _ = restarted.stop(&resumed.id).await;
}

#[tokio::test]
async fn resume_binding_persists_project_context_for_restart() {
    // F5: a resumed session's project context is restored from the persisted
    // binding, not re-detected. So the binding must carry `project_id` /
    // `is_linked_worktree` captured from the live session — verified here by
    // round-tripping through the store (record on native-id capture, read back
    // as explicit native-recovery metadata).
    let store = temp_store_path("resume-project-ctx");
    let worktree_root = store.parent().expect("store parent").join("worktrees");
    let agents_dir = temp_resumable_agents_dir("resume-project-ctx");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        store_path: Some(store),
        worktree_root: Some(worktree_root),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });
    let repo = init_git_repo("resume-project-ctx");
    let info = registry
        .create(SessionNewParams {
            cwd: Some(repo.clone()),
            ..resumable_params()
        })
        .await
        .expect("in-place session in the repo");
    let project_id = info.project_id.clone().expect("a project was stamped");
    assert_eq!(info.is_linked_worktree, Some(false), "the main checkout");

    // Capturing the native id persists the resume binding from live state.
    let recorded = registry
        .report_native_id(native_report!(&registry;
            session_id: info.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "native-resume".to_owned(),
            transcript_path: None,
        ))
        .await;
    assert!(recorded.recorded, "native id captured");

    let bindings = registry
        .projects()
        .expect("projects")
        .store()
        .load_resume()
        .expect("load resume bindings");
    assert_eq!(bindings.len(), 1, "exactly one resume binding persisted");
    assert_eq!(
        bindings[0].project_id.as_deref(),
        Some(project_id.as_str()),
        "project id is persisted so restart restores it without re-detecting"
    );
    assert_eq!(
        bindings[0].is_linked_worktree,
        Some(false),
        "the main-checkout flag is persisted too"
    );
}

#[tokio::test]
async fn concurrent_resize_and_recapture_keep_store_consistent_with_memory() {
    let store_path = temp_store_path("concurrent-persist");
    let agents_dir = temp_resumable_agents_dir("concurrent-persist");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(resumable_params())
        .await
        .expect("create session");
    let recorded = registry
        .report_native_id(native_report!(&registry;
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "native-concurrent".to_owned(),
            transcript_path: None,
        ))
        .await;
    assert!(recorded.recorded);

    // Race a resize against a second native-id report (SessionStart re-fires
    // on resume/clear/compact). The persisted binding must end at the live
    // size, never the pre-resize one: persist_resume_binding re-reads under
    // persist_lock, so whichever writer runs last reflects the resize.
    let resizer = {
        let registry = registry.clone();
        let id = created.id.clone();
        tokio::spawn(async move { registry.resize(&id, 200, 60).await })
    };
    let recapture = {
        let registry = registry.clone();
        let id = created.id.clone();
        tokio::spawn(async move {
            registry
                .report_native_id(native_report!(&registry;
                    session_id: id,
                    agent: "claude".to_owned(),
                    native_session_id: "native-concurrent".to_owned(),
                    transcript_path: None,
                ))
                .await
        })
    };
    resizer
        .await
        .expect("resize task")
        .expect("resize succeeds");
    recapture.await.expect("recapture task");

    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!((inspected.cols, inspected.rows), (200, 60));
    let persisted = crate::store::Store::new(store_path)
        .load_resume()
        .expect("load");
    assert_eq!(persisted.len(), 1, "no duplicate binding: {persisted:?}");
    assert_eq!(
        (persisted[0].cols, persisted[0].rows),
        (inspected.cols, inspected.rows),
        "persisted binding must match the live size after a concurrent resize + recapture"
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn resize_then_stop_leaves_no_binding() {
    let store_path = temp_store_path("resize-then-stop");
    let agents_dir = temp_resumable_agents_dir("resize-then-stop");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(resumable_params())
        .await
        .expect("create session");
    let recorded = registry
        .report_native_id(native_report!(&registry;
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "native-resize-stop".to_owned(),
            transcript_path: None,
        ))
        .await;
    assert!(recorded.recorded);
    registry
        .resize(&created.id, 90, 30)
        .await
        .expect("resize session");
    assert_eq!(
        crate::store::Store::new(store_path.clone())
            .load_resume()
            .expect("load")
            .len(),
        1,
        "resize must refresh the existing binding"
    );

    // Stopping after a resize must still drop the (resize-refreshed) binding.
    let stopped = registry.stop(&created.id).await.expect("stop");
    assert!(stopped.stopped);
    assert!(
        crate::store::Store::new(store_path)
            .load_resume()
            .expect("load")
            .is_empty(),
        "a resized-then-stopped session must not leave a resume binding"
    );
}

#[tokio::test]
async fn report_native_id_ignores_unknown_invalid_and_terminal() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });

    // Unknown session id.
    let unknown = registry
        .report_native_id(native_report!(&registry;
            session_id: SessionId("s-missing".to_owned()),
            agent: "claude".to_owned(),
            native_session_id: "native-1".to_owned(),
            transcript_path: None,
        ))
        .await;
    assert!(!unknown.recorded);

    let created = registry.create(params()).await.expect("create session");

    // Invalid (empty) native ids are rejected at the strict public boundary.
    SessionReportNativeIdParams::new(
        created.id.clone(),
        "runtime-invalid",
        "shell",
        1,
        ProcessStartIdentity::new(1),
        ReportSequence::new(1),
        native_report_expiry(),
        "",
        None,
    )
    .expect_err("an empty native id must fail validation");

    // Terminal session.
    let _ = registry.stop(&created.id).await;
    let terminal = registry
        .report_native_id(native_report!(&registry;
            session_id: created.id.clone(),
            agent: "shell".to_owned(),
            native_session_id: "native-late".to_owned(),
            transcript_path: None,
        ))
        .await;
    assert!(!terminal.recorded);
}

/// Input rules of a built-in runtime resolved through a default host.
fn builtin_input_rules(agent: &RuntimeRef, config: &SessionRegistryConfig) -> InputRules {
    super::input_rules_for_agent(&RuntimeHost::default(), agent, config)
}

/// A submit-delay override map holding one runtime.
fn submit_delay_override(runtime: &str, delay: Duration) -> BTreeMap<RuntimeId, Duration> {
    BTreeMap::from([(RuntimeId::parse(runtime).expect("runtime id"), delay)])
}

#[test]
fn claude_input_rules_use_configured_submit_delay() {
    let config = SessionRegistryConfig {
        shell_command: hermetic_shell(),
        submit_delay_overrides: submit_delay_override("claude", Duration::from_millis(75)),
        ..SessionRegistryConfig::default()
    };

    let rules = builtin_input_rules(&RuntimeRef::claude(), &config);

    assert!(!rules.bracketed_paste);
    assert_eq!(rules.submit_delay, Duration::from_millis(75));
}

/// A definition with an arbitrary runtime id, used to prove selection is by
/// definition data and never by a built-in id.
fn acme_definition(submit_delay_configurable: bool, prompt_arg: bool) -> RuntimeDefinition {
    RuntimeDefinition::new(DefinitionParts {
        runtime_id: RuntimeId::parse("acme").expect("runtime id"),
        origin: DefinitionOrigin::Builtin {
            package: Some(PackageIdentity {
                id: PackageId::parse("acme.runtime").expect("package id"),
                version: PackageVersion::parse("1.0.0").expect("package version"),
            }),
        },
        display_name: "Acme".to_owned(),
        program: LaunchProgram::Fixed("acme".to_owned()),
        default_args: Vec::new(),
        input_rules: InputRules::unrestricted(true, Duration::from_millis(150)),
        submit_delay_configurable,
        manifest: Arc::new(crate::detect::generic_shell_manifest().clone()),
        native: None,
        prompt_arg,
        version_probe_parser: None,
        version_probe_policy: None,
        integration: None,
    })
    .expect("valid definition")
}

#[test]
fn only_a_host_shell_runtime_is_not_an_agent_launch_root() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        ..SessionRegistryConfig::default()
    });

    assert!(!registry.launch_root_is_agent(&RuntimeRef::shell()));
    assert!(registry.launch_root_is_agent(&RuntimeRef::claude()));
    assert!(registry.launch_root_is_agent(&RuntimeRef::from_wire("acme")));
}

#[test]
fn submit_delay_override_applies_to_the_configured_runtime_id() {
    let config = SessionRegistryConfig {
        submit_delay_overrides: submit_delay_override("acme", Duration::from_millis(40)),
        ..SessionRegistryConfig::default()
    };

    let rules = super::input::input_rules_for_definition(&acme_definition(true, false), &config);

    assert!(rules.bracketed_paste);
    assert_eq!(rules.submit_delay, Duration::from_millis(40));
}

#[test]
fn submit_delay_override_is_ignored_unless_the_definition_allows_it() {
    let config = SessionRegistryConfig {
        submit_delay_overrides: BTreeMap::from([
            (
                RuntimeId::parse("acme").expect("runtime id"),
                Duration::from_millis(40),
            ),
            (
                RuntimeId::parse("codex").expect("runtime id"),
                Duration::from_millis(40),
            ),
        ]),
        ..SessionRegistryConfig::default()
    };

    let acme = super::input::input_rules_for_definition(&acme_definition(false, false), &config);
    let codex = builtin_input_rules(&RuntimeRef::codex(), &config);

    assert_eq!(acme.submit_delay, Duration::from_millis(150));
    assert_eq!(codex.submit_delay, Duration::from_millis(150));
}

#[test]
fn submit_delay_without_an_override_is_the_descriptor_value() {
    let config = SessionRegistryConfig::default();

    let rules = builtin_input_rules(&RuntimeRef::claude(), &config);
    let other_runtime_override = SessionRegistryConfig {
        submit_delay_overrides: submit_delay_override("acme", Duration::ZERO),
        ..SessionRegistryConfig::default()
    };

    assert_eq!(rules.submit_delay, Duration::from_millis(150));
    assert_eq!(
        builtin_input_rules(&RuntimeRef::claude(), &other_runtime_override).submit_delay,
        Duration::from_millis(150)
    );
}

#[test]
fn initial_prompt_is_a_launch_argument_only_when_the_definition_says_so() {
    let resolved = |prompt_arg| ResolvedAgent {
        name: "acme".to_owned(),
        base: RuntimeId::parse("acme").expect("valid runtime id"),
        definition: Arc::new(acme_definition(false, prompt_arg)),
        profile: None,
    };

    let as_argument = super::input::plan_initial_input_delivery(
        &resolved(true),
        pty_command("acme", []),
        Some("hello".to_owned()),
    );
    let typed = super::input::plan_initial_input_delivery(
        &resolved(false),
        pty_command("acme", []),
        Some("hello".to_owned()),
    );

    assert_eq!(as_argument.command.args, vec!["hello".to_owned()]);
    assert_eq!(as_argument.pending_initial_input, None);
    assert!(typed.command.args.is_empty());
    assert_eq!(typed.pending_initial_input.as_deref(), Some("hello"));
}

// ---------------------------------------------------------------------------
// session.diff
// ---------------------------------------------------------------------------
//
// The git-diff computation matrix below drives `diff::compute_session_diff`
// directly against a plain fixture repo (no live session/registry needed —
// it is a pure read over a worktree path). The base-precedence, hostile-ref,
// no-worktree, and unresolved-base tests drive the real `SessionRegistry::diff`
// entry point, reusing `project_registry`/`init_git_repo`/`git_in`/`params`.

#[test]
fn session_diff_modified_tracked_file_appears_as_a_change() {
    let repo = init_git_repo("diff-modified");
    std::fs::write(repo.join("README.md"), "modified\n").expect("modify tracked file");

    let result = super::diff::compute_session_diff(&repo, "main").expect("diff succeeds");

    assert!(!result.truncated);
    assert_eq!(result.base, "main");
    assert!(result.diff.contains("diff --git a/README.md b/README.md"));
    assert!(result.diff.contains("-init"));
    assert!(result.diff.contains("+modified"));
}

#[test]
fn session_diff_added_tracked_file_reflects_unstaged_edits_made_after_staging() {
    // `git diff <commit>` compares the commit tree directly to the working
    // tree (bypassing the index), so a staged-then-further-edited new file
    // must show its final, unstaged content — not just the staged blob.
    let repo = init_git_repo("diff-added");
    std::fs::write(repo.join("added.txt"), "staged content\n").expect("write added file");
    git_in(&repo, &["add", "added.txt"]);
    std::fs::write(
        repo.join("added.txt"),
        "staged content\nunstaged extra line\n",
    )
    .expect("unstaged edit atop the staged add");

    let result = super::diff::compute_session_diff(&repo, "main").expect("diff succeeds");

    assert!(!result.truncated);
    assert!(result.diff.contains("new file mode"));
    assert!(result.diff.contains("+staged content"));
    assert!(
        result.diff.contains("+unstaged extra line"),
        "git diff <commit> must reflect the working tree, not just the staged blob: {}",
        result.diff
    );
}

#[test]
fn session_diff_deleted_tracked_file_appears_as_a_deletion() {
    let repo = init_git_repo("diff-deleted");
    std::fs::remove_file(repo.join("README.md")).expect("delete tracked file from disk");

    let result = super::diff::compute_session_diff(&repo, "main").expect("diff succeeds");

    assert!(!result.truncated);
    assert!(result.diff.contains("deleted file mode"));
    assert!(result.diff.contains("-init"));
}

#[test]
fn session_diff_renamed_tracked_file_with_an_edit_is_detected_as_a_rename() {
    let repo = init_git_repo("diff-renamed");
    // A single-line file renamed and edited has too little byte overlap with
    // its original to clear git's default 50% rename-similarity threshold;
    // commit enough shared content first so the rename is actually detected.
    std::fs::write(
        repo.join("README.md"),
        "line one\nline two\nline three\nline four\nline five\n",
    )
    .expect("write substantial tracked content");
    git_in(&repo, &["add", "README.md"]);
    git_in(&repo, &["commit", "-q", "-m", "more content"]);

    git_in(&repo, &["mv", "README.md", "renamed.md"]);
    std::fs::write(
        repo.join("renamed.md"),
        "line one\nline two\nline three\nline four\nline five\nline six\n",
    )
    .expect("edit the renamed file");

    let result = super::diff::compute_session_diff(&repo, "main").expect("diff succeeds");

    assert!(!result.truncated);
    assert!(result.diff.contains("rename from README.md"));
    assert!(result.diff.contains("rename to renamed.md"));
}

#[test]
fn session_diff_untracked_text_file_appears_as_an_added_file_diff() {
    let repo = init_git_repo("diff-untracked-text");
    std::fs::write(repo.join("new.txt"), "hello\n").expect("write untracked file");

    let result = super::diff::compute_session_diff(&repo, "main").expect("diff succeeds");

    assert!(!result.truncated);
    assert!(result.diff.contains("diff --git a/new.txt b/new.txt"));
    assert!(result.diff.contains("new file mode"));
    assert!(result.diff.contains("+hello"));
}

#[test]
fn session_diff_binary_tracked_file_change_shows_gits_binary_stanza() {
    let repo = init_git_repo("diff-binary-tracked");
    std::fs::write(repo.join("bin.dat"), [0u8, 1, 2, 3, 255, 254]).expect("write binary file");
    git_in(&repo, &["add", "bin.dat"]);
    git_in(&repo, &["commit", "-q", "-m", "add binary"]);
    std::fs::write(repo.join("bin.dat"), [0u8, 9, 9, 9, 255, 254]).expect("modify binary file");

    let result = super::diff::compute_session_diff(&repo, "main").expect("diff succeeds");

    assert!(!result.truncated);
    assert!(result
        .diff
        .contains("Binary files a/bin.dat and b/bin.dat differ"));
}

#[test]
fn session_diff_untracked_binary_file_shows_gits_binary_stanza() {
    let repo = init_git_repo("diff-binary-untracked");
    std::fs::write(repo.join("new.bin"), [0u8, 1, 2, 255]).expect("write untracked binary file");

    let result = super::diff::compute_session_diff(&repo, "main").expect("diff succeeds");

    assert!(!result.truncated);
    assert!(result
        .diff
        .contains("Binary files /dev/null and b/new.bin differ"));
}

#[test]
fn cap_to_budget_stops_at_a_file_boundary_once_the_cap_is_exceeded() {
    // Two synthetic "files" shaped like real `diff --git` chunks: the first
    // fits comfortably under the cap, the second alone is already far over it.
    let first = format!("diff --git a/first b/first\n{}\n", "x".repeat(100));
    let second = format!(
        "diff --git a/second b/second\n{}\n",
        "y".repeat(protocol::MAX_SESSION_DIFF_BYTES)
    );
    let combined = format!("{first}{second}");

    let (capped, truncated) = super::diff::cap_to_budget(&combined);

    assert!(truncated);
    assert_eq!(
        capped, first,
        "must include the first file whole and stop exactly at the next file boundary"
    );
}

#[test]
fn session_diff_truncates_a_large_untracked_file_and_keeps_the_response_envelope_within_control_line_bytes(
) {
    let repo = init_git_repo("diff-truncate");
    // Comfortably over the cap once rendered as unified-diff `+` content; JSON
    // escaping only adds overhead on top of the raw byte count, never removes
    // it, so this is guaranteed to exceed `MAX_SESSION_DIFF_BYTES` once diffed.
    let huge = "x".repeat(protocol::MAX_SESSION_DIFF_BYTES + 4096);
    std::fs::write(repo.join("huge.txt"), &huge).expect("write huge untracked file");

    let result = super::diff::compute_session_diff(&repo, "main").expect("diff succeeds");

    assert!(
        result.truncated,
        "a file exceeding the cap on its own must truncate"
    );
    assert!(
        result.diff.is_empty(),
        "the only file did not fit at all, so nothing is included: {} bytes",
        result.diff.len()
    );

    let response = protocol::Response::ok(
        protocol::PROTOCOL_VERSION,
        "req-1",
        serde_json::to_value(&result).expect("serialize SessionDiffResult"),
    )
    .expect("valid response envelope");
    let serialized = serde_json::to_string(&response).expect("serialize Response envelope");
    assert!(
        serialized.len() < protocol::MAX_CONTROL_LINE_BYTES,
        "a truncated envelope must still fit one control line: {} bytes",
        serialized.len()
    );
}

#[test]
fn resolve_base_prefers_the_explicit_param_over_everything_else() {
    let resolved = super::diff::resolve_base(Some("explicit-ref".to_owned()), None, "s-none", None)
        .expect("an explicit base short-circuits before touching the store or a repository");

    assert_eq!(resolved, "explicit-ref");
}

#[test]
fn resolve_base_falls_back_to_the_repository_default_branch_when_no_store_or_binding_exists() {
    let repo = init_git_repo("diff-default-fallback");

    let resolved = super::diff::resolve_base(None, None, "s-none", Some(&repo))
        .expect("the repository's current branch resolves");

    assert_eq!(resolved, "main");
}

#[tokio::test]
async fn session_diff_session_without_a_worktree_fails_with_a_typed_error() {
    let registry = SessionRegistry::new(SessionRegistryConfig::default());
    let info = registry
        .create(params())
        .await
        .expect("plain session is created");

    let err = registry
        .diff(&info.id, None)
        .await
        .expect_err("a session with no worktree must fail session.diff");

    assert_eq!(err.code, "session_no_worktree");
    assert!(err.recover.is_some(), "must carry a recover hint: {err:?}");
}

#[tokio::test]
async fn session_diff_rejects_an_empty_explicit_base() {
    let registry = SessionRegistry::new(SessionRegistryConfig::default());

    let err = registry
        .diff(
            &SessionId("s-does-not-matter".to_owned()),
            Some(String::new()),
        )
        .await
        .expect_err("an empty base must be rejected before the session is even looked up");

    assert_eq!(err.code, "invalid_branch");
}

#[tokio::test]
async fn session_diff_rejects_a_dash_leading_explicit_base() {
    let registry = SessionRegistry::new(SessionRegistryConfig::default());

    let err = registry
        .diff(
            &SessionId("s-does-not-matter".to_owned()),
            Some("--upload-pack=evil".to_owned()),
        )
        .await
        .expect_err("a dash-leading base must be rejected as a possible argv flag injection");

    assert_eq!(err.code, "invalid_branch");
}

#[tokio::test]
async fn session_diff_rejects_a_control_character_in_the_explicit_base() {
    let registry = SessionRegistry::new(SessionRegistryConfig::default());

    let err = registry
        .diff(
            &SessionId("s-does-not-matter".to_owned()),
            Some("feat/x\u{7}".to_owned()),
        )
        .await
        .expect_err("a control character in the base must be rejected");

    assert_eq!(err.code, "invalid_branch");
}

#[tokio::test]
async fn session_diff_explicit_base_overrides_the_recorded_worktree_binding() {
    let (registry, repo) = project_registry("diff-explicit-base");
    git_in(&repo, &["branch", "release"]);

    let info = registry
        .create(SessionNewParams {
            repo: Some(repo.clone()),
            branch: Some("feat/explicit-base".to_owned()),
            ..params()
        })
        .await
        .expect("worktree-bound session is created");

    let result = registry
        .diff(&info.id, Some("release".to_owned()))
        .await
        .expect("diff succeeds with an explicit base");

    assert_eq!(result.base, "release");
}

#[tokio::test]
async fn session_diff_falls_back_to_the_recorded_worktree_base_branch_when_no_explicit_base_given()
{
    let (registry, repo) = project_registry("diff-recorded-base");
    git_in(&repo, &["branch", "release"]);

    let info = registry
        .create(SessionNewParams {
            repo: Some(repo.clone()),
            branch: Some("feat/recorded-base".to_owned()),
            base_branch: Some("release".to_owned()),
            ..params()
        })
        .await
        .expect("worktree-bound session is created with an explicit base branch");

    let result = registry
        .diff(&info.id, None)
        .await
        .expect("diff succeeds using the recorded base branch");

    assert_eq!(
        result.base, "release",
        "must use the worktree binding's recorded base branch, not the repository's current branch"
    );
}

#[tokio::test]
async fn session_diff_reports_a_typed_error_when_the_base_ref_does_not_resolve() {
    let (registry, repo) = project_registry("diff-bad-base");
    let info = registry
        .create(SessionNewParams {
            repo: Some(repo.clone()),
            branch: Some("feat/bad-base".to_owned()),
            ..params()
        })
        .await
        .expect("worktree-bound session is created");

    let err = registry
        .diff(
            &info.id,
            Some("totally-bogus-ref-that-does-not-exist".to_owned()),
        )
        .await
        .expect_err("an unresolvable base ref must fail, not silently succeed with an empty diff");

    assert_eq!(err.code, "session_diff_base_unresolved");
    assert!(!err.msg.is_empty());
}

#[tokio::test]
async fn session_diff_registry_end_to_end_reflects_worktree_changes_against_the_repository_default_base(
) {
    let (registry, repo) = project_registry("diff-e2e");
    let info = registry
        .create(SessionNewParams {
            repo: Some(repo.clone()),
            branch: Some("feat/e2e".to_owned()),
            ..params()
        })
        .await
        .expect("worktree-bound session is created");
    let worktree = info
        .worktree_path
        .clone()
        .expect("session must be worktree-bound for this test");

    std::fs::write(worktree.join("README.md"), "changed in the worktree\n")
        .expect("modify the tracked file inside the bound worktree");

    let result = registry.diff(&info.id, None).await.expect("diff succeeds");

    assert_eq!(result.base, "main");
    assert!(!result.truncated);
    assert!(result.diff.contains("README.md"));
    assert!(result.diff.contains("+changed in the worktree"));
}

// --- retention sweep (end to end) -----------------------------------------

/// A registry whose sessions exit immediately, with the metadata store and
/// worktree binding enabled so a sweep exercises the real removal path.
fn retention_registry(tag: &str) -> (SessionRegistry, PathBuf, PathBuf) {
    retention_registry_over(tag, Arc::new(ReadableHost::new()))
}

fn retention_registry_over(
    tag: &str,
    inspector: Arc<dyn ProcessInspector>,
) -> (SessionRegistry, PathBuf, PathBuf) {
    retention_registry_configured(tag, inspector, None)
}

/// [`retention_registry_over`] with a worker log directory that is a symlink
/// to a real directory, which log cleanup refuses, so a removal fails after
/// its marker sweep.
fn retention_registry_with_unusable_log_dir(
    tag: &str,
    inspector: Arc<dyn ProcessInspector>,
) -> (SessionRegistry, PathBuf, PathBuf) {
    let store = temp_store_path(tag);
    let data_dir = store.parent().expect("store parent").to_path_buf();
    let target = data_dir.join("log-target");
    fs::create_dir_all(&target).expect("create the symlink target");
    let link = data_dir.join("log-link");
    std::os::unix::fs::symlink(&target, &link).expect("create the log directory symlink");
    let (registry, worktree_root) = build_retention_registry(store, inspector, Some(link));
    (registry, data_dir, worktree_root)
}

fn retention_registry_configured(
    tag: &str,
    inspector: Arc<dyn ProcessInspector>,
    log_dir: Option<PathBuf>,
) -> (SessionRegistry, PathBuf, PathBuf) {
    let store = temp_store_path(tag);
    let data_dir = store.parent().expect("store parent").to_path_buf();
    let (registry, worktree_root) = build_retention_registry(store, inspector, log_dir);
    (registry, data_dir, worktree_root)
}

fn build_retention_registry(
    store: PathBuf,
    inspector: Arc<dyn ProcessInspector>,
    log_dir: Option<PathBuf>,
) -> (SessionRegistry, PathBuf) {
    let worktree_root = store.parent().expect("store parent").join("worktrees");
    let registry = SessionRegistry::new_with_inspector(
        SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "true"]),
            stop_grace: Duration::from_millis(50),
            store_path: Some(store),
            worktree_root: Some(worktree_root.clone()),
            log_dir,
            ..SessionRegistryConfig::default()
        },
        inspector,
    );
    (registry, worktree_root)
}

/// PID of the first scripted unreadable candidate of
/// [`UnreadableCandidateHost`]; candidate `n` counts down from it.
///
/// Above every PID Linux (`pid_max` is at most 2^22) or Darwin can assign, so
/// none names a real process that could be signalled.
const UNREADABLE_CANDIDATE_PID: Pid = Pid::MAX;

/// The [`ReadableHost`] view plus a scripted number of same-user processes
/// whose ownership markers cannot be read, and an optional process-table
/// failure.
#[derive(Debug, Default)]
struct UnreadableCandidateHost {
    readable: ReadableHost,
    candidates: AtomicUsize,
    listing_fails: AtomicBool,
}

impl UnreadableCandidateHost {
    fn set_candidate(&self, present: bool) {
        self.set_candidates(usize::from(present));
    }

    fn set_candidates(&self, count: usize) {
        self.candidates.store(count, Ordering::Release);
    }

    fn set_listing_fails(&self, fails: bool) {
        self.listing_fails.store(fails, Ordering::Release);
    }

    fn candidate_pid(index: usize) -> Pid {
        UNREADABLE_CANDIDATE_PID - Pid::try_from(index).expect("candidate index fits a pid")
    }

    /// Command name of scripted candidate `index`.
    fn candidate_comm(index: usize) -> String {
        if index == 0 {
            "unreadable".to_owned()
        } else {
            format!("unreadable-{index}")
        }
    }

    fn candidate_index(&self, pid: Pid) -> Option<usize> {
        let index = usize::try_from(UNREADABLE_CANDIDATE_PID.checked_sub(pid)?).ok()?;
        (index < self.candidates.load(Ordering::Acquire)).then_some(index)
    }

    fn candidate_fact(index: usize) -> ProcessFact {
        ProcessFact {
            pid: Self::candidate_pid(index),
            pgid: Self::candidate_pid(index),
            ppid: 1,
            start_identity: StartIdentity::new(1),
            comm: Self::candidate_comm(index),
            cmdline: Vec::new(),
        }
    }

    fn scripts_candidate(&self, pid: Pid) -> bool {
        self.candidate_index(pid).is_some()
    }
}

impl ProcessInspector for UnreadableCandidateHost {
    fn identity(&self, pid: Pid) -> Result<Option<ProcessIdentity>, crate::procwatch::Error> {
        // The candidate's start is never proven older than a worker, so only a
        // sweep without a worker bound still counts it as possibly marked.
        if self.scripts_candidate(pid) {
            return Ok(None);
        }
        self.readable.identity(pid)
    }

    fn is_running(&self, identity: ProcessIdentity) -> Result<bool, crate::procwatch::Error> {
        self.readable.is_running(identity)
    }

    fn parent_pid(&self, pid: Pid) -> Result<Option<Pid>, crate::procwatch::Error> {
        self.readable.parent_pid(pid)
    }

    fn process(&self, pid: Pid) -> Result<Option<ProcessFact>, crate::procwatch::Error> {
        match self.candidate_index(pid) {
            Some(index) => Ok(Some(Self::candidate_fact(index))),
            None => self.readable.process(pid),
        }
    }

    fn same_user_processes(&self) -> Result<Vec<ProcessFact>, crate::procwatch::Error> {
        if self.listing_fails.load(Ordering::Acquire) {
            return Err(crate::procwatch::Error::from_io(
                "test_listing",
                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            ));
        }
        let mut processes = self.readable.same_user_processes()?;
        for index in 0..self.candidates.load(Ordering::Acquire) {
            processes.push(Self::candidate_fact(index));
        }
        Ok(processes)
    }

    fn descendants(&self, root: Pid) -> Result<Vec<ProcessFact>, crate::procwatch::Error> {
        self.readable.descendants(root)
    }

    fn descendant_identities(
        &self,
        root: ProcessIdentity,
    ) -> Result<Vec<ProcessIdentity>, crate::procwatch::Error> {
        self.readable.descendant_identities(root)
    }

    fn cwd(&self, pid: Pid) -> Result<PathBuf, crate::procwatch::Error> {
        self.readable.cwd(pid)
    }

    fn executable(&self, pid: Pid) -> Result<Option<PathBuf>, crate::procwatch::Error> {
        self.readable.executable(pid)
    }

    fn exit_watch(&self, identity: ProcessIdentity) -> Result<ExitWatch, crate::procwatch::Error> {
        self.readable.exit_watch(identity)
    }

    fn ownership_markers(&self, pid: Pid) -> Result<OwnershipMarkers, crate::procwatch::Error> {
        if self.scripts_candidate(pid) {
            return Err(crate::procwatch::Error::from_io(
                "test_markers",
                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            ));
        }
        self.readable.ownership_markers(pid)
    }

    fn foreground_process_group(
        &self,
        root_pid: Pid,
    ) -> Result<Option<Pid>, crate::procwatch::Error> {
        self.readable.foreground_process_group(root_pid)
    }
}

/// The shortest policy the validator accepts, so a backdated session ages out
/// without the tests having to simulate a month of wall-clock time.
fn sweep_policy() -> SessionRetentionPolicy {
    SessionRetentionPolicy {
        enabled: true,
        sweep_interval_secs: 60,
        terminal_ttl_secs: 3_600,
        lost_ttl_secs: 3_600,
        max_removals_per_sweep: 25,
    }
}

/// Age well past [`sweep_policy`]'s TTLs.
fn past_ttl() -> time::Duration {
    time::Duration::hours(2)
}

fn backdated_stamp(age: time::Duration) -> String {
    (OffsetDateTime::now_utc() - age)
        .format(&Rfc3339)
        .expect("format backdated timestamp")
}

/// Create a session that exits on its own, optionally on its own worktree.
async fn exited_session(registry: &SessionRegistry, repo: Option<&PathBuf>) -> SessionInfo {
    let create = match repo {
        Some(repo) => SessionNewParams {
            cwd: None,
            repo: Some(repo.clone()),
            branch: Some(format!("feat/{}", uuid_like())),
            ..params()
        },
        None => params(),
    };
    let created = registry.create(create).await.expect("create session");
    registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("session exits")
}

/// Distinct branch names so two sessions can share one repository.
fn uuid_like() -> String {
    format!("x{}", TEMP_COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Backdate a terminal session so the terminal TTL selects it.
async fn age_terminal(registry: &SessionRegistry, id: &SessionId, age: time::Duration) {
    let mut sessions = registry.inner.sessions.lock().await;
    let entry = sessions.get_mut(id).expect("session entry");
    entry.info.updated_at = backdated_stamp(age);
}

/// Turn an exited session into a backdated `running` + `lost` one, the state a
/// session is left in when its worker disappears.
async fn age_lost(registry: &SessionRegistry, id: &SessionId, age: time::Duration) {
    let stamp = backdated_stamp(age);
    let mut sessions = registry.inner.sessions.lock().await;
    let entry = sessions.get_mut(id).expect("session entry");
    entry.info.state = SessionState::Running;
    entry.info.updated_at = stamp.clone();
    // The runtime's identity and generation are what the store's staleness guard
    // compares, so a lost runtime keeps the ones the worker was launched with.
    let runtime = entry
        .info
        .runtime
        .as_mut()
        .expect("a worker-backed session has a runtime");
    runtime.state = RuntimeState::Lost;
    runtime.last_connected_at = Some(stamp);
    runtime.loss_reason = Some("worker_unavailable".to_owned());
    entry.runtime = RuntimeHandle::Unavailable(RuntimeState::Lost);
}

async fn session_ids(registry: &SessionRegistry) -> Vec<String> {
    registry
        .list()
        .await
        .into_iter()
        .map(|info| info.id.0)
        .collect()
}

#[tokio::test]
async fn retention_sweep_removes_terminal_and_lost_sessions_and_keeps_an_in_ttl_one() {
    let (registry, _data_dir, _worktree_root) = retention_registry("sweep-e2e");
    let repo = init_git_repo("sweep-e2e");
    registry
        .set_retention_policy(sweep_policy())
        .await
        .expect("apply policy");

    let terminal = exited_session(&registry, None).await;
    age_terminal(&registry, &terminal.id, past_ttl()).await;
    let lost = exited_session(&registry, Some(&repo)).await;
    let lost_worktree = lost.worktree_path.clone().expect("lost session worktree");
    assert!(
        lost_worktree.is_dir(),
        "the worktree exists before the sweep"
    );
    age_lost(&registry, &lost.id, past_ttl()).await;
    let fresh = exited_session(&registry, None).await;

    let result = registry
        .sweep_retention(&protocol::SessionRetentionParams::default())
        .await
        .expect("sweep");

    assert_eq!(result.examined, 3, "{result:?}");
    assert_eq!(result.eligible, 2, "{result:?}");
    assert_eq!(result.held, 0, "{result:?}");
    assert_eq!(result.removed, 2, "{result:?}");
    assert_eq!(result.failed, 0, "{result:?}");
    assert_eq!(result.worktrees_cleaned, 1, "{result:?}");
    assert_eq!(result.worktrees_failed, 0, "{result:?}");
    assert!(
        result.sessions.iter().all(|candidate| candidate.removed),
        "every reported candidate was removed: {result:?}"
    );
    // DoD: the lost session's worktree is gone from disk, not merely reported.
    assert!(
        !lost_worktree.exists(),
        "the lost session's worktree must be gone: {}",
        lost_worktree.display()
    );
    assert_eq!(
        session_ids(&registry).await,
        vec![fresh.id.0.clone()],
        "only the in-TTL session survives"
    );
}

#[tokio::test]
async fn retention_sweep_keeps_a_session_whose_worktree_has_uncommitted_work() {
    let (registry, _data_dir, _worktree_root) = retention_registry("sweep-dirty");
    let repo = init_git_repo("sweep-dirty");
    registry
        .set_retention_policy(sweep_policy())
        .await
        .expect("apply policy");

    let session = exited_session(&registry, Some(&repo)).await;
    let worktree = session.worktree_path.clone().expect("session worktree");
    fs::write(
        worktree.join("README.md"),
        "work the agent did not commit\n",
    )
    .expect("dirty the worktree");
    age_lost(&registry, &session.id, past_ttl()).await;

    let result = registry
        .sweep_retention(&protocol::SessionRetentionParams::default())
        .await
        .expect("sweep");

    assert_eq!(result.eligible, 0, "{result:?}");
    assert_eq!(result.held, 1, "{result:?}");
    assert_eq!(result.removed, 0, "{result:?}");
    assert_eq!(result.worktrees_cleaned, 0, "{result:?}");
    assert_eq!(
        result.sessions.first().and_then(|candidate| candidate.hold),
        Some(protocol::SessionRetentionHold::WorktreeUncommitted),
        "{result:?}"
    );
    assert!(worktree.is_dir(), "the dirty worktree stays on disk");
    assert_eq!(session_ids(&registry).await, vec![session.id.0.clone()]);
}

#[tokio::test]
async fn retention_sweep_stops_at_the_removal_budget() {
    let (registry, _data_dir, _worktree_root) = retention_registry("sweep-budget");
    registry
        .set_retention_policy(sweep_policy())
        .await
        .expect("apply policy");

    for _ in 0..2 {
        let session = exited_session(&registry, None).await;
        age_terminal(&registry, &session.id, past_ttl()).await;
    }

    let result = registry
        .sweep_retention(&protocol::SessionRetentionParams {
            dry_run: false,
            limit: Some(1),
        })
        .await
        .expect("sweep");

    assert_eq!(result.eligible, 2, "{result:?}");
    assert_eq!(result.removed, 1, "{result:?}");
    assert_eq!(
        session_ids(&registry).await.len(),
        1,
        "the budget leaves the rest for the next sweep"
    );
}

#[tokio::test]
async fn retention_sweep_counts_a_removal_it_could_not_complete() {
    let (registry, data_dir, _worktree_root) = retention_registry("sweep-failed");
    registry
        .set_retention_policy(sweep_policy())
        .await
        .expect("apply policy");
    let session = exited_session(&registry, None).await;
    age_terminal(&registry, &session.id, past_ttl()).await;

    // Removal persists a record before it evicts the entry, so a data directory
    // it cannot write makes the real removal fail.
    fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o500))
        .expect("make the data directory read-only");
    let result = registry
        .sweep_retention(&protocol::SessionRetentionParams::default())
        .await
        .expect("sweep");
    fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o700))
        .expect("restore the data directory");

    assert_eq!(result.eligible, 1, "{result:?}");
    assert_eq!(result.removed, 0, "{result:?}");
    assert_eq!(result.failed, 1, "{result:?}");
    assert_eq!(
        session_ids(&registry).await,
        vec![session.id.0.clone()],
        "a failed removal leaves the session in place"
    );
}

/// Creates an exited session whose worker journals are deleted, so the record
/// is the only evidence of its runtime and its removal sweep has no worker
/// start bound.
async fn record_only_session(registry: &SessionRegistry) -> SessionInfo {
    record_only_session_in(registry, None).await
}

/// [`record_only_session`] on its own worktree of `repo`, when given.
async fn record_only_session_in(registry: &SessionRegistry, repo: Option<&PathBuf>) -> SessionInfo {
    let session = exited_session(registry, repo).await;
    let journals = registry
        .inner
        .config
        .worker_state_root
        .as_ref()
        .expect("worker state root")
        .join(&session.id.0);
    let mut removed_journals = 0;
    for journal in fs::read_dir(&journals).expect("read the session's journals") {
        let journal = journal.expect("journal entry").path();
        if journal
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            fs::remove_file(&journal).expect("remove the worker journal");
            removed_journals += 1;
        }
    }
    assert!(removed_journals > 0, "the worker journaled its runtime");
    session
}

/// Pins the removal of a runtime that only the record names (no worker
/// journal, so no worker start bounds its sweep): an unreadable same-user
/// process may carry its marker, so the removal stays refused, names the
/// process, and completes once that process is gone. A journaled runtime
/// dismisses the same process through its worker's start identity.
#[tokio::test]
async fn removal_of_a_record_only_runtime_is_refused_while_an_unreadable_process_remains() {
    let inspector = Arc::new(UnreadableCandidateHost::default());
    let (registry, _data_dir, _worktree_root) = retention_registry_over(
        "remove-record-only",
        Arc::clone(&inspector) as Arc<dyn ProcessInspector>,
    );
    let journaled = exited_session(&registry, None).await;
    let record_only = record_only_session(&registry).await;
    inspector.set_candidate(true);

    registry
        .remove(&journaled.id)
        .await
        .expect("the worker's start identity bounds a journaled runtime's sweep");
    let refused = registry
        .remove(&record_only.id)
        .await
        .expect_err("an unbounded sweep cannot dismiss an unreadable process");
    assert_eq!(
        refused.code,
        crate::runtime::lifecycle::SUPERVISION_AMBIGUOUS,
        "{refused:?}"
    );
    assert!(
        refused
            .msg
            .contains(&format!("pid {UNREADABLE_CANDIDATE_PID} ")),
        "the refusal names the candidate: {refused:?}"
    );
    assert!(
        refused.msg.contains("`unreadable`"),
        "the refusal names the candidate's command: {refused:?}"
    );
    assert_eq!(
        refused.recover.as_deref(),
        Some(super::supervision::UNREADABLE_CANDIDATES_RECOVER),
        "{refused:?}"
    );
    assert_eq!(
        session_ids(&registry).await,
        vec![record_only.id.0.clone()],
        "a refused removal leaves the session in place"
    );

    inspector.set_candidate(false);
    registry
        .remove(&record_only.id)
        .await
        .expect("the removal completes once no unreadable process remains");
    assert!(session_ids(&registry).await.is_empty());
}

/// The refusal lists at most the documented number of candidates and counts
/// the rest.
#[tokio::test]
async fn removal_refusal_bounds_the_listed_unreadable_processes() {
    let inspector = Arc::new(UnreadableCandidateHost::default());
    let (registry, _data_dir, _worktree_root) = retention_registry_over(
        "remove-many-unreadable",
        Arc::clone(&inspector) as Arc<dyn ProcessInspector>,
    );
    let record_only = record_only_session(&registry).await;
    let cap = super::supervision::MAX_LISTED_UNREADABLE_CANDIDATES;
    let extra = 3;
    inspector.set_candidates(cap + extra);

    let refused = registry
        .remove(&record_only.id)
        .await
        .expect_err("unreadable processes refuse the removal");
    let listed = refused.msg.matches("pid ").count();
    assert_eq!(listed, cap, "{refused:?}");
    assert!(
        refused.msg.ends_with(&format!("and {extra} more")),
        "{refused:?}"
    );
    assert!(refused.recover.is_some(), "{refused:?}");
}

/// A refusal caused by something other than unreadable processes names none
/// and the detailed outcome reports the other blocker.
#[tokio::test]
async fn removal_refusal_from_a_sweep_failure_lists_no_unreadable_process() {
    let inspector = Arc::new(UnreadableCandidateHost::default());
    let (registry, _data_dir, _worktree_root) = retention_registry_over(
        "remove-sweep-failure",
        Arc::clone(&inspector) as Arc<dyn ProcessInspector>,
    );
    let record_only = record_only_session(&registry).await;
    inspector.set_candidate(true);
    inspector.set_listing_fails(true);

    let refused = registry
        .remove(&record_only.id)
        .await
        .expect_err("a failing process listing refuses the removal");
    assert_eq!(
        refused.code,
        crate::runtime::lifecycle::SUPERVISION_AMBIGUOUS,
        "{refused:?}"
    );
    assert!(!refused.msg.contains("pid "), "{refused:?}");
    assert!(!refused.msg.contains("unreadable"), "{refused:?}");
    assert_eq!(refused.recover, None, "{refused:?}");

    let outcome = registry
        .sweep_lost_runtime_detailed(&record_only.id.0, "rt-detail", None)
        .await;
    assert_eq!(outcome.cleanup, super::supervision::Cleanup::Unconfirmed);
    assert!(outcome.other_blockers);
    assert!(outcome.unreadable.is_empty());
    assert!(!outcome.only_unreadable_candidates_blocked());

    inspector.set_listing_fails(false);
    let outcome = registry
        .sweep_lost_runtime_detailed(&record_only.id.0, "rt-detail", None)
        .await;
    assert_eq!(outcome.cleanup, super::supervision::Cleanup::Unconfirmed);
    assert!(!outcome.other_blockers);
    assert_eq!(outcome.unreadable.len(), 1);
    assert!(outcome.only_unreadable_candidates_blocked());
}

/// Consent lets a removal proceed past unreadable-marker processes only:
/// it reports them, never signals them, and deletes the session with its
/// worktree.
#[tokio::test]
async fn accepted_unconfirmed_cleanup_removes_a_session_blocked_only_by_unreadable_processes() {
    let inspector = Arc::new(UnreadableCandidateHost::default());
    let (registry, _data_dir, _worktree_root) = retention_registry_over(
        "remove-accept-unreadable",
        Arc::clone(&inspector) as Arc<dyn ProcessInspector>,
    );
    let repo = init_git_repo("remove-accept-unreadable");
    let session = record_only_session_in(&registry, Some(&repo)).await;
    let worktree = session.worktree_path.clone().expect("session worktree");
    assert!(worktree.exists(), "the session owns a checkout");
    inspector.set_candidate(true);

    let removed = registry
        .remove_with(&session.id, super::UnconfirmedCleanup::Accept)
        .await
        .expect("consent accepts the unreadable candidate");

    assert!(removed.removed, "{removed:?}");
    assert_eq!(removed.worktrees_removed, 1, "{removed:?}");
    assert_eq!(
        removed.accepted_unconfirmed_processes,
        vec![protocol::UnconfirmedProcess {
            pid: UNREADABLE_CANDIDATE_PID,
            start_identity: protocol::ProcessStartIdentity::new(1),
            command: Some("unreadable".to_owned()),
        }],
        "{removed:?}"
    );
    assert!(session_ids(&registry).await.is_empty());
    assert!(!worktree.exists(), "the checkout is deleted");
}

/// The accepted processes travel in a result built after the cleanup, so a
/// removal with more candidates than fit one response is refused before
/// anything is deleted; exactly the maximum is still accepted.
#[tokio::test]
async fn accepted_unconfirmed_cleanup_is_bounded_before_anything_is_deleted() {
    let inspector = Arc::new(UnreadableCandidateHost::default());
    let (registry, _data_dir, _worktree_root) = retention_registry_over(
        "remove-accept-bounded",
        Arc::clone(&inspector) as Arc<dyn ProcessInspector>,
    );
    let repo = init_git_repo("remove-accept-bounded");
    let session = record_only_session_in(&registry, Some(&repo)).await;
    let worktree = session.worktree_path.clone().expect("session worktree");
    let cap = super::supervision::MAX_ACCEPTED_UNCONFIRMED_PROCESSES;

    inspector.set_candidates(cap + 1);
    let refused = registry
        .remove_with(&session.id, super::UnconfirmedCleanup::Accept)
        .await
        .expect_err("more candidates than one result can list refuse the removal");
    assert_eq!(
        refused.code,
        crate::runtime::lifecycle::SUPERVISION_AMBIGUOUS,
        "{refused:?}"
    );
    assert!(refused.recover.is_some(), "{refused:?}");
    assert_eq!(session_ids(&registry).await, vec![session.id.0.clone()]);
    assert!(worktree.exists(), "a refused removal deletes nothing");

    inspector.set_candidates(cap);
    let removed = registry
        .remove_with(&session.id, super::UnconfirmedCleanup::Accept)
        .await
        .expect("exactly the maximum is accepted");
    assert_eq!(removed.accepted_unconfirmed_processes.len(), cap);
    assert!(session_ids(&registry).await.is_empty());
}

/// Sends `method` with the bare session id as params through the request
/// handler and returns the reply.
async fn remove_over_the_wire(
    registry: &SessionRegistry,
    method: &str,
    id: &SessionId,
) -> Result<serde_json::Value, protocol::ProtocolError> {
    let state = DaemonState::new(
        HealthInfo::new("test"),
        registry.clone(),
        Arc::new(crate::governance::HostGovernanceService::open_test()),
        crate::test_support::overlay_registry(),
    );
    let request = Request::new("remove-1", method, serde_json::json!(id)).expect("valid request");
    crate::api::handle_request(&request, &state)
        .await
        .into_result()
}

/// `session.remove` never consents; `session.remove_accepting_unconfirmed`
/// takes the same bare id and accepts the unreadable candidate.
#[tokio::test]
async fn the_remove_methods_differ_only_in_consent_to_unreadable_processes() {
    let inspector = Arc::new(UnreadableCandidateHost::default());
    let (registry, _data_dir, _worktree_root) = retention_registry_over(
        "remove-methods",
        Arc::clone(&inspector) as Arc<dyn ProcessInspector>,
    );
    let session = record_only_session(&registry).await;
    inspector.set_candidate(true);

    let refused = remove_over_the_wire(&registry, protocol::method::SESSION_REMOVE, &session.id)
        .await
        .expect_err("session.remove refuses an unreadable candidate");
    assert_eq!(
        refused.code,
        crate::runtime::lifecycle::SUPERVISION_AMBIGUOUS,
        "{refused:?}"
    );
    assert_eq!(session_ids(&registry).await, vec![session.id.0.clone()]);

    let payload = remove_over_the_wire(
        &registry,
        protocol::method::SESSION_REMOVE_ACCEPTING_UNCONFIRMED,
        &session.id,
    )
    .await
    .expect("the accepting method removes the session");
    let removed: protocol::SessionRemoveResult =
        serde_json::from_value(payload).expect("remove result");
    assert!(removed.removed, "{removed:?}");
    assert_eq!(
        removed.accepted_unconfirmed_processes.len(),
        1,
        "{removed:?}"
    );
    assert_eq!(
        removed.accepted_unconfirmed_processes[0].pid,
        UNREADABLE_CANDIDATE_PID
    );
    assert!(session_ids(&registry).await.is_empty());
}

/// A removal logs the processes it accepted only once it proceeds; a
/// refusal logs no acceptance.
#[tokio::test]
async fn accepted_processes_are_logged_only_when_the_removal_proceeds() {
    let logs = crate::runtime::lifecycle::tests::LogCapture::default();
    let _subscriber = logs.install();
    let inspector = Arc::new(UnreadableCandidateHost::default());
    let (registry, _data_dir, _worktree_root) = retention_registry_over(
        "remove-accept-logging",
        Arc::clone(&inspector) as Arc<dyn ProcessInspector>,
    );
    let session = record_only_session(&registry).await;
    let accepted_log = "removal proceeds past an unreadable-marker process";

    inspector.set_candidates(super::supervision::MAX_ACCEPTED_UNCONFIRMED_PROCESSES + 1);
    registry
        .remove_with(&session.id, super::UnconfirmedCleanup::Accept)
        .await
        .expect_err("more candidates than one removal can accept refuse it");
    assert!(
        !logs.text().contains(accepted_log),
        "a refused removal accepted nothing: {}",
        logs.text()
    );

    inspector.set_candidate(true);
    inspector.set_listing_fails(true);
    registry
        .remove_with(&session.id, super::UnconfirmedCleanup::Accept)
        .await
        .expect_err("another blocker refuses the removal");
    assert!(
        !logs.text().contains(accepted_log),
        "a refusal by another blocker accepted nothing: {}",
        logs.text()
    );

    inspector.set_listing_fails(false);
    registry
        .remove_with(&session.id, super::UnconfirmedCleanup::Accept)
        .await
        .expect("the removal proceeds");
    assert_eq!(logs.text().matches(accepted_log).count(), 1);
}

/// A removal that fails after the sweep and after its checkout was deleted
/// still logged that it proceeded past the candidate; the retry logs again
/// for its own attempt.
#[tokio::test]
async fn a_removal_that_fails_after_the_sweep_still_logs_that_it_proceeded() {
    let logs = crate::runtime::lifecycle::tests::LogCapture::default();
    let _subscriber = logs.install();
    let inspector = Arc::new(UnreadableCandidateHost::default());
    let (registry, data_dir, _worktree_root) = retention_registry_with_unusable_log_dir(
        "remove-accept-late-failure",
        Arc::clone(&inspector) as Arc<dyn ProcessInspector>,
    );
    let repo = init_git_repo("remove-accept-late-failure");
    let session = record_only_session_in(&registry, Some(&repo)).await;
    let worktree = session.worktree_path.clone().expect("session worktree");
    assert!(worktree.exists(), "the session owns a checkout");
    inspector.set_candidate(true);
    let proceeds_log = "removal proceeds past an unreadable-marker process";

    let failed = registry
        .remove_with(&session.id, super::UnconfirmedCleanup::Accept)
        .await
        .expect_err("log cleanup fails after the checkout was deleted");
    assert_eq!(failed.code, "session_log_cleanup_failed", "{failed:?}");
    assert!(
        !worktree.exists(),
        "the checkout was deleted before the failure"
    );
    assert_eq!(session_ids(&registry).await, vec![session.id.0.clone()]);
    assert_eq!(
        logs.text().matches(proceeds_log).count(),
        1,
        "{}",
        logs.text()
    );

    fs::remove_file(data_dir.join("log-link")).expect("remove the log directory symlink");
    fs::create_dir(data_dir.join("log-link")).expect("replace it with a real directory");
    registry
        .remove_with(&session.id, super::UnconfirmedCleanup::Accept)
        .await
        .expect("the retry completes the removal");
    assert_eq!(logs.text().matches(proceeds_log).count(), 2);
}

/// A removal that needs no consent reports no accepted processes even when
/// the caller consented.
#[tokio::test]
async fn accepted_unconfirmed_cleanup_reports_nothing_when_no_process_was_accepted() {
    let inspector = Arc::new(UnreadableCandidateHost::default());
    let (registry, _data_dir, _worktree_root) = retention_registry_over(
        "remove-accept-nothing",
        Arc::clone(&inspector) as Arc<dyn ProcessInspector>,
    );
    let session = record_only_session(&registry).await;

    let removed = registry
        .remove_with(&session.id, super::UnconfirmedCleanup::Accept)
        .await
        .expect("a confirmed sweep removes the session");

    assert!(removed.removed, "{removed:?}");
    assert!(
        removed.accepted_unconfirmed_processes.is_empty(),
        "{removed:?}"
    );
}

/// Without consent the same candidate refuses, whether the caller passes the
/// default or names the refusal.
#[tokio::test]
async fn unconfirmed_cleanup_is_refused_unless_accepted() {
    let inspector = Arc::new(UnreadableCandidateHost::default());
    let (registry, _data_dir, _worktree_root) = retention_registry_over(
        "remove-refuse-unreadable",
        Arc::clone(&inspector) as Arc<dyn ProcessInspector>,
    );
    let session = record_only_session(&registry).await;
    inspector.set_candidate(true);

    for refused in [
        registry.remove(&session.id).await,
        registry
            .remove_with(&session.id, super::UnconfirmedCleanup::Refuse)
            .await,
    ] {
        let error = refused.expect_err("an unreadable candidate refuses without consent");
        assert_eq!(
            error.code,
            crate::runtime::lifecycle::SUPERVISION_AMBIGUOUS,
            "{error:?}"
        );
    }
    assert_eq!(session_ids(&registry).await, vec![session.id.0.clone()]);
}

/// Consent covers unreadable candidates only: a sweep that also failed to
/// inspect the process table stays refused and keeps the session.
#[tokio::test]
async fn accepted_unconfirmed_cleanup_still_refuses_another_blocker() {
    let inspector = Arc::new(UnreadableCandidateHost::default());
    let (registry, _data_dir, _worktree_root) = retention_registry_over(
        "remove-accept-other-blocker",
        Arc::clone(&inspector) as Arc<dyn ProcessInspector>,
    );
    let session = record_only_session(&registry).await;
    inspector.set_candidate(true);
    inspector.set_listing_fails(true);

    let refused = registry
        .remove_with(&session.id, super::UnconfirmedCleanup::Accept)
        .await
        .expect_err("a sweep failure is not covered by consent");
    assert_eq!(
        refused.code,
        crate::runtime::lifecycle::SUPERVISION_AMBIGUOUS,
        "{refused:?}"
    );
    assert!(!refused.msg.contains("pid "), "{refused:?}");
    assert_eq!(session_ids(&registry).await, vec![session.id.0.clone()]);
}

/// Consent is per call: a refused removal keeps its durable intent, and a
/// retry with consent completes it.
#[tokio::test]
async fn a_refused_removal_is_completed_by_a_retry_with_consent() {
    let inspector = Arc::new(UnreadableCandidateHost::default());
    let (registry, _data_dir, _worktree_root) = retention_registry_over(
        "remove-retry-with-consent",
        Arc::clone(&inspector) as Arc<dyn ProcessInspector>,
    );
    let session = record_only_session(&registry).await;
    inspector.set_candidate(true);

    registry
        .remove(&session.id)
        .await
        .expect_err("the first removal is refused");
    assert_eq!(session_ids(&registry).await, vec![session.id.0.clone()]);
    registry
        .remove(&session.id)
        .await
        .expect_err("consent from an earlier call is not remembered");

    let removed = registry
        .remove_with(&session.id, super::UnconfirmedCleanup::Accept)
        .await
        .expect("the retry with consent completes the removal");
    assert!(removed.removed, "{removed:?}");
    assert_eq!(removed.accepted_unconfirmed_processes.len(), 1);
    assert!(session_ids(&registry).await.is_empty());
}

/// The retention sweep never consents: a session it selects but whose sweep
/// is blocked by an unreadable process counts as failed and stays, and only
/// an explicit consenting removal deletes it.
#[tokio::test]
async fn retention_sweep_never_consents_to_unconfirmed_cleanup() {
    let inspector = Arc::new(UnreadableCandidateHost::default());
    let (registry, _data_dir, _worktree_root) = retention_registry_over(
        "sweep-never-consents",
        Arc::clone(&inspector) as Arc<dyn ProcessInspector>,
    );
    registry
        .set_retention_policy(sweep_policy())
        .await
        .expect("apply policy");
    let session = record_only_session(&registry).await;
    age_terminal(&registry, &session.id, past_ttl()).await;
    inspector.set_candidate(true);

    let result = registry
        .sweep_retention(&protocol::SessionRetentionParams::default())
        .await
        .expect("sweep");

    assert_eq!(result.eligible, 1, "{result:?}");
    assert_eq!(result.removed, 0, "{result:?}");
    assert_eq!(result.failed, 1, "{result:?}");
    assert_eq!(session_ids(&registry).await, vec![session.id.0.clone()]);

    registry
        .remove_with(&session.id, super::UnconfirmedCleanup::Accept)
        .await
        .expect("an explicit consenting removal deletes the kept session");
    assert!(session_ids(&registry).await.is_empty());
}

#[tokio::test]
async fn retention_sweep_reports_a_worktree_it_could_not_delete() {
    let (registry, _data_dir, worktree_root) = retention_registry("sweep-debris");
    let repo = init_git_repo("sweep-debris");
    registry
        .set_retention_policy(sweep_policy())
        .await
        .expect("apply policy");
    let session = exited_session(&registry, Some(&repo)).await;
    let worktree = session.worktree_path.clone().expect("session worktree");
    age_terminal(&registry, &session.id, past_ttl()).await;

    // `git worktree remove` has to unlink the checkout's directory entry, which
    // a read-only worktree root refuses while still allowing the safety probe to
    // read the checkout.
    fs::set_permissions(&worktree_root, fs::Permissions::from_mode(0o500))
        .expect("make the worktree root read-only");
    let result = registry
        .sweep_retention(&protocol::SessionRetentionParams::default())
        .await
        .expect("sweep");
    fs::set_permissions(&worktree_root, fs::Permissions::from_mode(0o700))
        .expect("restore the worktree root");

    assert_eq!(result.removed, 1, "{result:?}");
    assert_eq!(
        result.worktrees_cleaned, 0,
        "a checkout still on disk is never counted as cleaned: {result:?}"
    );
    assert_eq!(result.worktrees_failed, 1, "{result:?}");
    assert!(
        worktree.exists(),
        "the debris this sweep reported is really there: {}",
        worktree.display()
    );
}

#[tokio::test]
async fn concurrent_sweeps_are_serialised_by_the_sweep_lock() {
    let (registry, _data_dir, _worktree_root) = retention_registry("sweep-lock");
    registry
        .set_retention_policy(sweep_policy())
        .await
        .expect("apply policy");
    for _ in 0..2 {
        let session = exited_session(&registry, None).await;
        age_terminal(&registry, &session.id, past_ttl()).await;
    }

    let sweep_params = protocol::SessionRetentionParams::default();
    let (first, second) = tokio::join!(
        registry.sweep_retention(&sweep_params),
        registry.sweep_retention(&sweep_params)
    );
    let first = first.expect("first sweep");
    let second = second.expect("second sweep");

    let mut eligible = [first.eligible, second.eligible];
    eligible.sort_unstable();
    assert_eq!(
        eligible, [0, 2],
        "the second sweep starts after the first finished, so it selects nothing: {first:?} {second:?}"
    );
    assert_eq!(
        first.removed + second.removed,
        2,
        "each session is removed exactly once: {first:?} {second:?}"
    );
    assert!(session_ids(&registry).await.is_empty());
}

#[tokio::test]
async fn the_worktree_probe_budget_holds_every_candidate_it_could_not_inspect() {
    let (registry, _data_dir, _worktree_root) = retention_registry("sweep-probe-budget");
    // One candidate past the per-sweep probe budget. The paths need not exist:
    // what is pinned is that an uninspected candidate is held, not removed.
    let budget = super::retention::MAX_WORKTREE_PROBES_PER_SWEEP;
    let mut candidates = (0..=budget)
        .map(|index| protocol::SessionRetentionCandidate {
            session_id: SessionId(format!("s-probe-{index}")),
            reason: protocol::SessionRetentionReason::Terminal,
            age_secs: 7_200,
            worktree_path: Some(PathBuf::from(format!(
                "/nonexistent/worktrees/s-probe-{index}"
            ))),
            hold: None,
            removed: false,
        })
        .collect::<Vec<_>>();

    registry.apply_worktree_holds(&mut candidates).await;

    let held = candidates
        .iter()
        .filter(|candidate| candidate.hold.is_some())
        .count();
    assert_eq!(
        held, 1,
        "only the candidate past the budget is held: {candidates:?}"
    );
    assert_eq!(
        candidates[budget].hold,
        Some(protocol::SessionRetentionHold::WorktreeUnknown),
        "an uninspected candidate is unproven, so it is kept"
    );
}

fn scripted_registry(
    config: SessionRegistryConfig,
) -> (
    SessionRegistry,
    Arc<crate::runtime::lifecycle::tests::ScriptedSupervisor>,
) {
    let mut config = config;
    let (runtime_root, state_root) = test_worker_roots(&config);
    config.worker_runtime_root = Some(runtime_root.clone());
    config.worker_state_root = Some(state_root.clone());
    config.supervision = Some(super::test_supervision(&runtime_root, &state_root));
    let mut supervisor = crate::runtime::lifecycle::tests::ScriptedSupervisor::over(
        crate::runtime::InProcessWorkerLauncher::new(runtime_root, state_root),
    );
    if let Some(store) = config.store_path.clone() {
        supervisor = supervisor.recording_store(store);
    }
    let supervisor = Arc::new(supervisor);
    let registry = SessionRegistry::new_with_launcher_and_inspector(
        config,
        Arc::clone(&supervisor) as Arc<dyn crate::runtime::WorkerLauncher>,
        Arc::new(ReadableHost::new()),
    );
    (registry, supervisor)
}

fn stored_record(store_path: &std::path::Path, id: &SessionId) -> crate::store::SessionRecord {
    crate::store::Store::new(store_path.to_path_buf())
        .load_sessions()
        .expect("load session records")
        .into_iter()
        .find(|record| record.session_id == id.0)
        .expect("session record")
}

/// Bound on waiting for a detached registration to commit: a hang guard.
const DETACHED_COMMIT_TIMEOUT: Duration = HANG_GUARD;

#[tokio::test]
async fn create_persists_the_generation_before_starting_its_job() {
    let store_path = temp_store_path("lifecycle-persist-first");
    let (registry, supervisor) = scripted_registry(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        ..SessionRegistryConfig::default()
    });

    let created = registry.create(params()).await.expect("create session");

    let started = supervisor.started();
    let [job] = started.as_slice() else {
        panic!("exactly one generation starts: {started:?}");
    };
    let key = pohunek_platform::supervisor::WorkerKey::from_service_id(job).expect("worker job");
    assert_eq!(key.session_id(), created.id.0);
    assert_eq!(
        supervisor.persisted_at_start(),
        [Some(key.generation().to_owned())],
        "the preparing record names the generation before it is registered"
    );
    let record = stored_record(&store_path, &created.id);
    assert_eq!(record.runtime.service_id.as_deref(), Some(job.as_str()));
    assert_eq!(record.runtime.generation.as_deref(), Some(key.generation()));
    assert_eq!(
        record.runtime.executable.as_deref(),
        Some(std::path::Path::new("/nonexistent/pohunek-sessiond"))
    );
    registry.remove(&created.id).await.expect("remove session");
    assert_eq!(
        supervisor.retired(),
        std::slice::from_ref(job),
        "removal retires the exact job"
    );
    assert!(supervisor.live_jobs().await.is_empty());
}

#[tokio::test]
async fn dropping_create_after_start_still_converges_to_one_generation() {
    let (registry, supervisor) = scripted_registry(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let gate = crate::runtime::lifecycle::tests::StartGate {
        session_id: None,
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    };
    supervisor.hold_start(gate.clone());

    let creating = tokio::spawn({
        let registry = registry.clone();
        async move { registry.create(params()).await }
    });
    gate.entered.notified().await;
    creating.abort();
    assert!(
        creating
            .await
            .expect_err("create was dropped")
            .is_cancelled(),
        "the client-facing create future is gone"
    );
    gate.release.notify_one();

    let session = tokio::time::timeout(DETACHED_COMMIT_TIMEOUT, async {
        loop {
            let sessions = registry.list().await;
            if let [session] = sessions.as_slice() {
                if session
                    .runtime
                    .as_ref()
                    .is_some_and(|runtime| runtime.state == RuntimeState::Live)
                {
                    return session.clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the detached registration commits the session");

    let started = supervisor.started();
    assert_eq!(started.len(), 1, "no second generation: {started:?}");
    assert_eq!(supervisor.live_jobs().await, started);
    assert!(supervisor.retired().is_empty());
    registry.stop(&session.id).await.expect("stop session");
}

#[tokio::test]
async fn unavailable_supervision_during_create_leaves_a_reconnecting_session() {
    let store_path = temp_store_path("lifecycle-unavailable");
    let (registry, supervisor) = scripted_registry(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        store_path: Some(store_path.clone()),
        ..SessionRegistryConfig::default()
    });
    supervisor.script_starts([crate::runtime::lifecycle::tests::StartStep::Fail]);
    supervisor.script_inspects([crate::runtime::lifecycle::tests::InspectStep::Unavailable]);

    let error = registry
        .create(params())
        .await
        .expect_err("supervisor outage");

    assert_eq!(
        error.code,
        crate::runtime::lifecycle::SUPERVISION_UNAVAILABLE
    );
    assert!(supervisor.retired().is_empty(), "nothing is killed");
    let sessions = registry.list().await;
    let [session] = sessions.as_slice() else {
        panic!("the session stays visible for reconciliation: {sessions:?}");
    };
    let runtime = session.runtime.as_ref().expect("runtime");
    assert_eq!(runtime.state, RuntimeState::Reconnecting);
    assert_eq!(
        runtime.loss_reason.as_deref(),
        Some(crate::runtime::lifecycle::SUPERVISION_UNAVAILABLE)
    );
    let record = stored_record(&store_path, &session.id);
    assert_eq!(record.runtime.state, RuntimeState::Reconnecting);
    assert_eq!(
        record.runtime.service_id.as_deref(),
        supervisor
            .started()
            .first()
            .map(pohunek_platform::supervisor::ServiceId::as_str),
        "the untouched job stays recorded"
    );
}

/// A scripted registry that binds worktrees, with a `session.new` on
/// `branch` held at its job start.
///
/// At the hold the worktree is bound and the preparing record names the
/// generation, so a test can arrange how the rest of the create ends before
/// releasing it.
struct HeldWorktreeCreate {
    registry: SessionRegistry,
    supervisor: Arc<crate::runtime::lifecycle::tests::ScriptedSupervisor>,
    store_path: PathBuf,
    repo: PathBuf,
    creating: tokio::task::JoinHandle<Result<SessionInfo, protocol::ProtocolError>>,
    release: Arc<tokio::sync::Notify>,
    id: SessionId,
    service_id: pohunek_platform::supervisor::ServiceId,
    worktree: PathBuf,
}

impl HeldWorktreeCreate {
    async fn start(tag: &str, branch: &str) -> Self {
        let store_path = temp_store_path(tag);
        let worktree_root = store_path.parent().expect("store parent").join("worktrees");
        let repo = init_git_repo(tag);
        let (registry, supervisor) = scripted_registry(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path.clone()),
            worktree_root: Some(worktree_root),
            ..SessionRegistryConfig::default()
        });
        let gate = crate::runtime::lifecycle::tests::StartGate {
            session_id: None,
            entered: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
        };
        supervisor.hold_start(gate.clone());
        let creating = tokio::spawn({
            let registry = registry.clone();
            let params = SessionNewParams {
                cwd: None,
                repo: Some(repo.clone()),
                branch: Some(branch.to_owned()),
                ..params()
            };
            async move { registry.create(params).await }
        });
        gate.entered.notified().await;
        let records = crate::store::Store::new(store_path.clone())
            .load_sessions()
            .expect("load preparing record");
        let [record] = records.as_slice() else {
            panic!("one preparing record: {records:?}");
        };
        let service_id = pohunek_platform::supervisor::ServiceId::parse(
            record
                .runtime
                .service_id
                .clone()
                .expect("the preparing record names its job"),
        )
        .expect("service id");
        let worktree = record
            .info
            .worktree_path
            .clone()
            .expect("the preparing record names its worktree");
        // Uncommitted work a forced removal would destroy.
        std::fs::write(worktree.join("uncommitted.txt"), "agent work").expect("write work");
        Self {
            id: SessionId(record.session_id.clone()),
            registry,
            supervisor,
            store_path,
            repo,
            creating,
            release: gate.release,
            service_id,
            worktree,
        }
    }

    /// Makes the commit of the held create's session entry fail.
    fn fail_entry_commit(&self) {
        self.registry
            .inner
            .store
            .as_ref()
            .expect("registry store")
            .fail_next_write_before_rename();
    }

    /// Makes every later inspection and retirement of the held job fail as
    /// an unreachable manager, so no retry can prove it ended.
    fn keep_supervisor_unavailable(&self) {
        self.supervisor.script_job(
            self.service_id.clone(),
            crate::runtime::lifecycle::tests::JobScript::Unavailable,
        );
    }

    /// The create's outcome as its client sees it.
    async fn outcome(&mut self) -> Result<SessionInfo, protocol::ProtocolError> {
        tokio::time::timeout(DETACHED_COMMIT_TIMEOUT, &mut self.creating)
            .await
            .expect("create finishes")
            .expect("create task joins")
    }

    /// Drops the client-facing create future while its job start is held.
    async fn drop_client(&mut self) {
        self.creating.abort();
        assert!(
            (&mut self.creating)
                .await
                .expect_err("create was dropped")
                .is_cancelled(),
            "the client-facing create future is gone"
        );
    }

    /// Waits until the detached create transaction released the session.
    async fn settled(&self) {
        drop(
            tokio::time::timeout(
                DETACHED_COMMIT_TIMEOUT,
                self.registry.lock_lifecycle(&self.id),
            )
            .await
            .expect("the detached create transaction finishes"),
        );
    }

    fn binding_exists(&self) -> bool {
        crate::store::Store::new(self.store_path.clone())
            .load_worktrees()
            .expect("load worktree bindings")
            .iter()
            .any(|binding| binding.session_id == self.id.0)
    }

    fn record_exists(&self) -> bool {
        crate::store::Store::new(self.store_path.clone())
            .load_sessions()
            .expect("load session records")
            .iter()
            .any(|record| record.session_id == self.id.0)
    }

    fn assert_worktree_kept(&self) {
        assert_eq!(
            std::fs::read_to_string(self.worktree.join("uncommitted.txt")).ok(),
            Some("agent work".to_owned()),
            "a possibly live runtime keeps its checkout and work"
        );
        assert!(self.binding_exists(), "the worktree binding is kept");
        assert!(
            self.record_exists(),
            "the record is kept for reconciliation"
        );
    }

    async fn assert_worktree_removed_and_branch_reusable(&self, branch: &str) {
        assert!(
            !self.worktree.exists(),
            "the proven-ended create's worktree is removed: {}",
            self.worktree.display()
        );
        assert!(!self.binding_exists(), "the worktree binding is dropped");
        assert!(!self.record_exists(), "the preparing record is deleted");
        assert!(
            self.registry.list().await.is_empty(),
            "nothing of the failed create stays listed"
        );
        let retried = self
            .registry
            .create(SessionNewParams {
                cwd: None,
                repo: Some(self.repo.clone()),
                branch: Some(branch.to_owned()),
                ..params()
            })
            .await
            .expect("the freed branch binds again");
        self.registry
            .stop(&retried.id)
            .await
            .expect("stop the retried session");
    }

    async fn assert_reconnecting(&self) {
        let sessions = self.registry.list().await;
        let [session] = sessions.as_slice() else {
            panic!("the session stays visible for reconciliation: {sessions:?}");
        };
        assert_eq!(session.id, self.id);
        let runtime = session.runtime.as_ref().expect("runtime");
        assert_eq!(runtime.state, RuntimeState::Reconnecting);
        assert_eq!(session.worktree_path.as_ref(), Some(&self.worktree));
    }
}

#[tokio::test]
async fn failed_entry_commit_with_an_unconfirmed_retire_keeps_the_worktree() {
    let mut held = HeldWorktreeCreate::start("wt-commit-unconfirmed", "feat/kept").await;
    held.fail_entry_commit();
    held.keep_supervisor_unavailable();
    held.release.notify_one();

    let error = held.outcome().await.expect_err("the entry cannot commit");

    assert_eq!(
        error.code,
        crate::runtime::lifecycle::SUPERVISION_UNAVAILABLE
    );
    assert!(
        error.msg.contains("keeps its worktree")
            && error.msg.contains(&held.worktree.display().to_string()),
        "the error names the kept worktree: {error:?}"
    );
    assert_eq!(held.supervisor.retired(), [held.service_id.clone()]);
    held.assert_worktree_kept();
    held.assert_reconnecting().await;
}

#[tokio::test]
async fn failed_entry_commit_with_a_confirmed_retire_removes_the_worktree() {
    let mut held = HeldWorktreeCreate::start("wt-commit-retired", "feat/retired").await;
    held.fail_entry_commit();
    held.release.notify_one();

    let error = held.outcome().await.expect_err("the entry cannot commit");

    assert_eq!(error.code, "session_store_failed", "got: {error:?}");
    assert_eq!(held.supervisor.retired(), [held.service_id.clone()]);
    held.assert_worktree_removed_and_branch_reusable("feat/retired")
        .await;
}

#[tokio::test]
async fn dropped_create_ending_cleaned_still_removes_its_worktree() {
    let mut held = HeldWorktreeCreate::start("wt-dropped-cleaned", "feat/dropped").await;
    held.drop_client().await;
    held.fail_entry_commit();
    held.release.notify_one();

    held.settled().await;

    assert_eq!(held.supervisor.retired(), [held.service_id.clone()]);
    held.assert_worktree_removed_and_branch_reusable("feat/dropped")
        .await;
}

#[tokio::test]
async fn dropped_create_with_an_unconfirmed_retire_keeps_its_worktree() {
    let mut held = HeldWorktreeCreate::start("wt-dropped-unconfirmed", "feat/kept-dropped").await;
    held.drop_client().await;
    held.fail_entry_commit();
    held.keep_supervisor_unavailable();
    held.release.notify_one();

    held.settled().await;

    assert_eq!(held.supervisor.retired(), [held.service_id.clone()]);
    held.assert_worktree_kept();
    held.assert_reconnecting().await;
}

#[tokio::test]
async fn unconfirmed_create_is_compensated_with_its_worktree_once_its_job_ends() {
    let mut held = HeldWorktreeCreate::start("wt-retry-compensated", "feat/retried").await;
    let (barrier, mut passes) = tokio::sync::mpsc::unbounded_channel();
    held.registry
        .inner
        .supervision_retries
        .state
        .lock()
        .expect("supervision retry state is never poisoned")
        .pass_finished = Some(barrier);
    // The job start fails before any worker runs, so no worker journal
    // exists, and the supervisor cannot tell what is left of the job.
    held.supervisor
        .script_starts([crate::runtime::lifecycle::tests::StartStep::Fail]);
    held.keep_supervisor_unavailable();
    held.release.notify_one();

    let error = held.outcome().await.expect_err("supervisor outage");
    assert_eq!(
        error.code,
        crate::runtime::lifecycle::SUPERVISION_UNAVAILABLE
    );
    assert!(held.supervisor.retired().is_empty(), "nothing is retired");
    held.assert_worktree_kept();
    held.assert_reconnecting().await;

    // The supervisor answers again: the job ended without a process.
    held.supervisor.script_job(
        held.service_id.clone(),
        crate::runtime::lifecycle::tests::JobScript::Present {
            state: pohunek_platform::supervisor::ServiceState::Failed,
            process: None,
            definition: None,
        },
    );
    // A pass that ran before the supervisor recovered leaves the session
    // pending; the first pass that sees the ended job compensates it.
    tokio::time::timeout(DETACHED_COMMIT_TIMEOUT, async {
        loop {
            let (pass, resume) = passes.recv().await.expect("retry barrier open");
            resume
                .send(())
                .expect("the retry loop waits at the barrier");
            if pass == held.id && !held.record_exists() {
                return;
            }
        }
    })
    .await
    .expect("the supervision retry compensates the ended create");

    assert_eq!(held.supervisor.retired(), [held.service_id.clone()]);
    held.assert_worktree_removed_and_branch_reusable("feat/retried")
        .await;
}

/// A worktree-binding daemon configuration whose store and worker roots are
/// fixed, so a registry built again from it models a daemon restart.
struct RestartableDaemon {
    config: SessionRegistryConfig,
    store_path: PathBuf,
    repo: PathBuf,
}

impl RestartableDaemon {
    fn new(tag: &str) -> Self {
        let store_path = temp_store_path(tag);
        let worktree_root = store_path.parent().expect("store parent").join("worktrees");
        let mut config = SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path.clone()),
            worktree_root: Some(worktree_root),
            ..SessionRegistryConfig::default()
        };
        let (runtime_root, state_root) = test_worker_roots(&config);
        config.supervision = Some(super::test_supervision(&runtime_root, &state_root));
        config.worker_runtime_root = Some(runtime_root);
        config.worker_state_root = Some(state_root);
        Self {
            config,
            store_path,
            repo: init_git_repo(tag),
        }
    }

    /// The configuration with a shell program that does not exist, so a
    /// create fails building its launch command after binding its target.
    fn without_shell(&self) -> SessionRegistryConfig {
        SessionRegistryConfig {
            shell_command: ShellCommand::new(
                "/nonexistent/pohunek-no-such-shell",
                std::iter::empty::<String>(),
            ),
            ..self.config.clone()
        }
    }

    fn params(&self, branch: &str) -> SessionNewParams {
        SessionNewParams {
            cwd: None,
            repo: Some(self.repo.clone()),
            branch: Some(branch.to_owned()),
            ..params()
        }
    }

    fn store(&self) -> crate::store::Store {
        crate::store::Store::new(self.store_path.clone())
    }

    fn records(&self) -> Vec<crate::store::SessionRecord> {
        self.store().load_sessions().expect("load session records")
    }

    fn bindings(&self) -> Vec<crate::store::WorktreeBinding> {
        self.store()
            .load_worktrees()
            .expect("load worktree bindings")
    }

    /// The only durable record, which must be a create intent naming no
    /// worker generation, and the only binding, which must be its worktree.
    fn intent_and_worktree(&self) -> (SessionId, PathBuf) {
        let records = self.records();
        let [record] = records.as_slice() else {
            panic!("exactly one create record: {records:?}");
        };
        let transaction = record.transaction.as_ref().expect("create transaction");
        assert_eq!(transaction.kind, crate::store::TransactionKind::Create);
        assert_eq!(transaction.phase, "preparing");
        assert_eq!(record.runtime.generation, None, "nothing was launched");
        let bindings = self.bindings();
        let [binding] = bindings.as_slice() else {
            panic!("exactly one worktree binding: {bindings:?}");
        };
        assert_eq!(binding.session_id, record.session_id);
        (SessionId(record.session_id.clone()), binding.path.clone())
    }

    /// Restarts the daemon over the same store and worker roots and runs
    /// its startup reconciliation.
    async fn restart(&self) -> SessionRegistry {
        let (registry, _supervisor) = scripted_registry(self.config.clone());
        registry
            .reconcile_workers()
            .await
            .expect("startup reconciliation");
        registry
    }

    /// Asserts that nothing of a compensated create is left and that its
    /// branch binds again.
    async fn assert_compensated(
        &self,
        registry: &SessionRegistry,
        worktree: &std::path::Path,
        branch: &str,
    ) {
        assert!(
            !worktree.exists(),
            "the compensated create's worktree is removed: {}",
            worktree.display()
        );
        assert!(self.bindings().is_empty(), "its binding is dropped");
        assert!(self.records().is_empty(), "its record is deleted last");
        assert!(registry.list().await.is_empty(), "nothing stays listed");
        let retried = registry
            .create(self.params(branch))
            .await
            .expect("the freed branch binds again");
        registry
            .stop(&retried.id)
            .await
            .expect("stop the retried session");
    }
}

/// Arms `registry` to park its next create once the target is bound.
fn hold_bound_create(registry: &SessionRegistry) -> crate::runtime::lifecycle::tests::StartGate {
    let gate = crate::runtime::lifecycle::tests::StartGate {
        session_id: None,
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    };
    *registry
        .inner
        .bound_create_hold
        .lock()
        .expect("bound create hold is never poisoned") = Some(gate.clone());
    gate
}

#[tokio::test]
async fn create_killed_between_bind_and_launch_is_compensated_at_restart() {
    let daemon = RestartableDaemon::new("wt-crash-after-bind");
    let (registry, supervisor) = scripted_registry(daemon.config.clone());
    let gate = hold_bound_create(&registry);
    // The daemon's own runtime: shutting it down drops the create's task at
    // its current await point, as a killed daemon would.
    let doomed = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build the doomed daemon runtime");
    doomed.spawn({
        let registry = registry.clone();
        let params = daemon.params("feat/crashed");
        async move { registry.create(params).await }
    });
    gate.entered.notified().await;

    let (_id, worktree) = daemon.intent_and_worktree();
    assert!(worktree.is_dir(), "the target is bound before the launch");
    doomed.shutdown_background();
    drop(registry);
    assert!(supervisor.started().is_empty(), "no job ever started");

    let restarted = daemon.restart().await;

    daemon
        .assert_compensated(&restarted, &worktree, "feat/crashed")
        .await;
}

#[tokio::test]
async fn daemon_shutdown_drains_an_in_flight_create() {
    let daemon = RestartableDaemon::new("wt-drain");
    let (registry, _supervisor) = scripted_registry(daemon.config.clone());
    let gate = hold_bound_create(&registry);
    let creating = tokio::spawn({
        let registry = registry.clone();
        let params = daemon.params("feat/drained");
        async move { registry.create(params).await }
    });
    gate.entered.notified().await;
    let (id, worktree) = daemon.intent_and_worktree();

    registry.begin_daemon_shutdown();
    let refused = registry
        .create(daemon.params("feat/refused"))
        .await
        .expect_err("shutdown refuses a new create");
    assert_eq!(refused.code, "daemon_shutting_down");
    assert_eq!(daemon.records().len(), 1, "a refused create writes nothing");
    let mut drain = Box::pin(registry.drain_creates());
    assert!(
        futures::poll!(drain.as_mut()).is_pending(),
        "the drain waits for the in-flight create"
    );
    gate.release.notify_one();

    assert!(drain.await, "the drain ends once the create settled");
    let created = creating
        .await
        .expect("create task joins")
        .expect("the drained create commits");
    assert_eq!(created.id, id);
    let record = stored_record(&daemon.store_path, &id);
    assert_eq!(record.transaction, None, "the create committed");
    assert_eq!(record.info.worktree_path.as_ref(), Some(&worktree));
    assert_eq!(daemon.bindings().len(), 1, "its worktree stays owned");
}

/// Connect deadline and drain timeout an already-exited worker must never
/// wait for.
const NEVER: Duration = Duration::from_secs(600);

/// Bound on a create or drain that must not wait for [`NEVER`].
const PROMPT: Duration = Duration::from_secs(20);

/// A registry whose workers run as direct children of an executable that
/// exits before binding its socket.
fn exiting_worker_registry(
    tag: &str,
) -> (
    SessionRegistry,
    Arc<crate::runtime::lifecycle::tests::ScriptedSupervisor>,
    PathBuf,
) {
    let store_path = temp_store_path(tag);
    let mut config = SessionRegistryConfig {
        shell_command: hermetic_shell(),
        store_path: Some(store_path.clone()),
        worker_connect_deadline: NEVER,
        create_drain_timeout: NEVER,
        ..SessionRegistryConfig::default()
    };
    let (runtime_root, state_root) = test_worker_roots(&config);
    let executable = crate::runtime::lifecycle::tests::write_exiting_worker(
        store_path.parent().expect("store directory"),
    );
    config.worker_runtime_root = Some(runtime_root.clone());
    config.worker_state_root = Some(state_root.clone());
    config.supervision = Some(
        crate::runtime::SubprocessWorkerEnvironment {
            runtime_home: runtime_root.clone(),
            state_home: state_root.clone(),
            data_home: state_root.clone(),
            config_home: state_root.clone(),
            cache_home: state_root.clone(),
            home: state_root,
            daemon_socket: runtime_root.join("test-daemon.sock"),
        }
        .supervision(executable)
        .with_environment_source(crate::test_support::thread_environment_source()),
    );
    let supervisor = Arc::new(crate::runtime::lifecycle::tests::ScriptedSupervisor::over(
        crate::runtime::SubprocessWorkerLauncher::new(),
    ));
    let registry = SessionRegistry::new_with_launcher_and_inspector(
        config,
        Arc::clone(&supervisor) as Arc<dyn crate::runtime::WorkerLauncher>,
        Arc::new(ReadableHost::new()),
    );
    (registry, supervisor, store_path)
}

#[tokio::test]
async fn daemon_shutdown_does_not_wait_the_connect_deadline_for_an_exited_worker() {
    let (registry, supervisor, store_path) = exiting_worker_registry("drain-exited");
    let gate = crate::runtime::lifecycle::tests::StartGate {
        session_id: None,
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    };
    supervisor.hold_start(gate.clone());
    let creating = tokio::spawn({
        let registry = registry.clone();
        async move { registry.create(params()).await }
    });
    gate.entered.notified().await;

    registry.begin_daemon_shutdown();
    let mut drain = Box::pin(registry.drain_creates());
    assert!(
        futures::poll!(drain.as_mut()).is_pending(),
        "the drain waits for the in-flight create"
    );
    gate.release.notify_one();
    let started = Instant::now();

    assert!(
        tokio::time::timeout(PROMPT, drain)
            .await
            .expect("shutdown does not wait the worker connect deadline"),
        "the create settled before the drain timeout"
    );
    assert!(
        started.elapsed() < PROMPT,
        "shutdown latency {:?} must not depend on the {NEVER:?} connect deadline",
        started.elapsed()
    );
    let error = creating
        .await
        .expect("create task joins")
        .expect_err("the worker never serves");
    assert_eq!(
        error.code,
        crate::runtime::lifecycle::WORKER_EXITED_BEFORE_READY
    );
    assert!(error.msg.contains("exit code 3"), "{}", error.msg);
    assert_eq!(supervisor.retired(), supervisor.started());
    assert!(
        crate::store::Store::new(store_path)
            .load_sessions()
            .expect("load session records")
            .is_empty(),
        "the failed create is compensated"
    );
}

#[tokio::test]
async fn create_refuses_a_worker_socket_that_cannot_be_bound_before_writing_anything() {
    let store_path = temp_store_path("socket-too-long");
    let limit = pohunek_paths::Platform::current()
        .expect("supported test platform")
        .socket_path_max_bytes();
    let base = pohunek_test_support::temp_root();
    // Allocated session IDs are `s-` plus a 26-character ULID.
    let published_tail = format!(
        "/s-{}/{}",
        "0".repeat(26),
        pohunek_paths::WORKER_SOCKET_NAME
    );
    // The published socket sits exactly at the limit, so only the longer
    // staged bind path is over it.
    let runtime_root =
        base.join("p".repeat(limit - base.as_os_str().len() - published_tail.len() - 1));
    let (registry, supervisor) = scripted_registry(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        store_path: Some(store_path.clone()),
        worker_runtime_root: Some(runtime_root.clone()),
        ..SessionRegistryConfig::default()
    });

    let error = registry
        .create(params())
        .await
        .expect_err("the worker could never bind its socket");

    assert_eq!(
        error.code,
        crate::runtime::lifecycle::WORKER_SOCKET_PATH_INVALID
    );
    assert_eq!(error.class, ErrorClass::Configuration);
    assert!(
        error.msg.contains(&runtime_root.display().to_string()),
        "{}",
        error.msg
    );
    assert!(error.msg.contains(".s0000000000000000"), "{}", error.msg);
    assert!(error.msg.contains(&limit.to_string()), "{}", error.msg);
    assert!(error.recover.is_some(), "{error:?}");
    assert!(supervisor.calls().is_empty(), "no worker was launched");
    assert!(
        !store_path.exists(),
        "the create is refused before its intent record is written"
    );
}

#[tokio::test]
async fn create_drain_gives_up_at_its_deadline() {
    let daemon = RestartableDaemon::new("wt-drain-deadline");
    let (registry, _supervisor) = scripted_registry(SessionRegistryConfig {
        create_drain_timeout: Duration::ZERO,
        ..daemon.config.clone()
    });
    let gate = hold_bound_create(&registry);
    let creating = tokio::spawn({
        let registry = registry.clone();
        let params = daemon.params("feat/undrained");
        async move { registry.create(params).await }
    });
    gate.entered.notified().await;

    registry.begin_daemon_shutdown();
    assert!(
        !registry.drain_creates().await,
        "a create still running at the deadline is left to reconciliation"
    );
    daemon.intent_and_worktree();

    gate.release.notify_one();
    let created = creating
        .await
        .expect("create task joins")
        .expect("the create commits after the drain gave up");
    registry.stop(&created.id).await.expect("stop the session");
}

/// Arms the supervision retry barrier of `registry`: each finished pass is
/// reported with the handle that resumes the retry loop.
fn retry_passes(
    registry: &SessionRegistry,
) -> tokio::sync::mpsc::UnboundedReceiver<(SessionId, tokio::sync::oneshot::Sender<()>)> {
    let (barrier, passes) = tokio::sync::mpsc::unbounded_channel();
    registry
        .inner
        .supervision_retries
        .state
        .lock()
        .expect("supervision retry state is never poisoned")
        .pass_finished = Some(barrier);
    passes
}

/// Asserts that `id` is the only listed session and that it waits for its
/// create compensation to be retried.
async fn assert_compensation_pending(registry: &SessionRegistry, id: &SessionId) {
    let sessions = registry.list().await;
    let [session] = sessions.as_slice() else {
        panic!("the failed create stays listed for the retry: {sessions:?}");
    };
    assert_eq!(&session.id, id);
    let runtime = session.runtime.as_ref().expect("runtime");
    assert_eq!(runtime.state, RuntimeState::Reconnecting);
    assert_eq!(
        runtime.loss_reason.as_deref(),
        Some(super::supervision::CREATE_COMPENSATION_PENDING)
    );
}

/// Resumes retry passes of `id` until one leaves no durable record, then
/// asserts that the running `registry` released everything of the create.
async fn await_compensated(
    daemon: &RestartableDaemon,
    registry: &SessionRegistry,
    passes: &mut tokio::sync::mpsc::UnboundedReceiver<(
        SessionId,
        tokio::sync::oneshot::Sender<()>,
    )>,
    id: &SessionId,
    worktree: &std::path::Path,
) {
    tokio::time::timeout(DETACHED_COMMIT_TIMEOUT, async {
        loop {
            let (pass, resume) = passes.recv().await.expect("retry barrier open");
            resume
                .send(())
                .expect("the retry loop waits at the barrier");
            if pass == *id && daemon.records().is_empty() {
                return;
            }
        }
    })
    .await
    .expect("the supervision retry compensates the create");
    assert!(!worktree.exists(), "the checkout is removed");
    assert!(daemon.bindings().is_empty(), "its binding is dropped");
    assert!(
        registry.list().await.is_empty(),
        "the compensated create leaves the running registry"
    );
}

#[tokio::test]
async fn launch_build_failure_with_a_failing_binding_store_is_compensated_by_the_retry() {
    let daemon = RestartableDaemon::new("wt-build-store-fail");
    let registry = SessionRegistry::new(daemon.without_shell());
    let mut passes = retry_passes(&registry);
    registry
        .inner
        .store
        .as_ref()
        .expect("registry store")
        .fail_next_binding_removal();

    let error = registry
        .create(daemon.params("feat/store-fail"))
        .await
        .expect_err("the launch command cannot be built");

    assert_eq!(error.code, "agent_binary_missing", "got: {error:?}");
    assert!(
        error.msg.contains("keeps its create record")
            && error.msg.contains("create_compensation_pending"),
        "the error names the kept record and its retry: {error:?}"
    );
    let (id, worktree) = daemon.intent_and_worktree();
    assert!(
        !worktree.exists(),
        "the checkout is removed even though its binding could not be dropped"
    );
    assert_compensation_pending(&registry, &id).await;

    // The binding store recovered, so the retry drops the binding of the
    // vanished checkout and deletes the record, without a daemon restart.
    await_compensated(&daemon, &registry, &mut passes, &id, &worktree).await;

    // A launchable daemon binds the freed branch again.
    daemon
        .assert_compensated(&daemon.restart().await, &worktree, "feat/store-fail")
        .await;
}

#[tokio::test]
async fn failed_worktree_removal_is_retried_until_the_checkout_is_removable() {
    let daemon = RestartableDaemon::new("wt-remove-fail");
    let registry = SessionRegistry::new(daemon.without_shell());
    let mut passes = retry_passes(&registry);
    let gate = hold_bound_create(&registry);
    let creating = tokio::spawn({
        let registry = registry.clone();
        let params = daemon.params("feat/locked");
        async move { registry.create(params).await }
    });
    gate.entered.notified().await;
    let (id, worktree) = daemon.intent_and_worktree();
    // A locked worktree refuses `git worktree remove --force`.
    let worktree_arg = worktree.to_str().expect("utf-8 worktree path");
    git_in(&daemon.repo, &["worktree", "lock", worktree_arg]);
    gate.release.notify_one();

    let error = creating
        .await
        .expect("create task joins")
        .expect_err("the launch command cannot be built");

    assert_eq!(error.code, "agent_binary_missing", "got: {error:?}");
    assert!(
        worktree.is_dir(),
        "the checkout that could not be removed stays"
    );
    daemon.intent_and_worktree();
    assert_compensation_pending(&registry, &id).await;

    // A pass while the worktree is still locked changes nothing.
    let (pass, resume) = tokio::time::timeout(DETACHED_COMMIT_TIMEOUT, passes.recv())
        .await
        .expect("a retry pass runs")
        .expect("retry barrier open");
    assert_eq!(pass, id);
    assert!(worktree.is_dir(), "the locked checkout stays");
    daemon.intent_and_worktree();
    assert_compensation_pending(&registry, &id).await;

    git_in(&daemon.repo, &["worktree", "unlock", worktree_arg]);
    resume
        .send(())
        .expect("the retry loop waits at the barrier");
    await_compensated(&daemon, &registry, &mut passes, &id, &worktree).await;

    // A launchable daemon binds the freed branch again.
    daemon
        .assert_compensated(&daemon.restart().await, &worktree, "feat/locked")
        .await;
}

#[tokio::test]
async fn unfinished_create_compensation_is_repeated_at_restart() {
    let daemon = RestartableDaemon::new("wt-remove-fail-restart");
    let registry = SessionRegistry::new(daemon.without_shell());
    let gate = hold_bound_create(&registry);
    let creating = tokio::spawn({
        let registry = registry.clone();
        let params = daemon.params("feat/locked-restart");
        async move { registry.create(params).await }
    });
    gate.entered.notified().await;
    let (id, worktree) = daemon.intent_and_worktree();
    let worktree_arg = worktree.to_str().expect("utf-8 worktree path");
    git_in(&daemon.repo, &["worktree", "lock", worktree_arg]);
    gate.release.notify_one();
    creating
        .await
        .expect("create task joins")
        .expect_err("the launch command cannot be built");
    assert_compensation_pending(&registry, &id).await;
    // The daemon stops before its retry succeeded.
    registry.begin_daemon_shutdown();
    drop(registry);
    git_in(&daemon.repo, &["worktree", "unlock", worktree_arg]);

    let restarted = daemon.restart().await;

    daemon
        .assert_compensated(&restarted, &worktree, "feat/locked-restart")
        .await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "the dead-create setup, the failed retirement, and the retried settlement stay visible end to end"
)]
async fn reconciled_create_whose_worker_ended_is_retired_then_compensated() {
    let daemon = RestartableDaemon::new("wt-terminal-create");
    let (registry, supervisor) = scripted_registry(daemon.config.clone());
    let created = registry
        .create(daemon.params("feat/ended"))
        .await
        .expect("create session");
    let launched = stored_record(&daemon.store_path, &created.id).runtime;
    registry.stop(&created.id).await.expect("stop session");
    let worktree = created.worktree_path.clone().expect("bound worktree");
    registry.begin_daemon_shutdown();
    drop(registry);
    // The daemon died after the worker ran, before it committed the create:
    // the worker still serves the final output of the ended runtime.
    let mut record = stored_record(&daemon.store_path, &created.id);
    record.transaction = Some(crate::store::SessionTransaction {
        id: format!("create-{}", created.id.0),
        kind: crate::store::TransactionKind::Create,
        phase: "preparing".to_owned(),
        previous_worker_id: None,
        previous_worker_instance_id: None,
        daemon_instance_id: None,
    });
    record.desired_state = crate::store::DesiredState::Running;
    record.runtime = crate::store::RuntimeRecord {
        state: RuntimeState::Starting,
        ..launched
    };
    record.info.state = SessionState::Starting;
    if let Some(runtime) = record.info.runtime.as_mut() {
        runtime.state = RuntimeState::Starting;
    }
    daemon
        .store()
        .record_session(&record)
        .expect("persist the preparing create");
    let service_id = pohunek_platform::supervisor::ServiceId::parse(
        record
            .runtime
            .service_id
            .clone()
            .expect("the record names its job"),
    )
    .expect("service id");
    supervisor.script_retire_unavailable(service_id.clone());
    // The create's own connect wait inspects its job too; only calls from the
    // restarted daemon are asserted below.
    let created_calls = supervisor.calls();
    let [crate::runtime::lifecycle::tests::Call::Start(_), created_waits @ ..] =
        created_calls.as_slice()
    else {
        panic!("the create started exactly its job first");
    };
    assert!(created_waits
        .iter()
        .all(|call| matches!(call, crate::runtime::lifecycle::tests::Call::Inspect(_))));
    let before_restart = 1 + created_waits.len();

    let restarted = SessionRegistry::new_with_launcher_and_inspector(
        daemon.config.clone(),
        Arc::clone(&supervisor) as Arc<dyn crate::runtime::WorkerLauncher>,
        Arc::new(ReadableHost::new()),
    );
    let (barrier, mut passes) = tokio::sync::mpsc::unbounded_channel();
    restarted
        .inner
        .supervision_retries
        .state
        .lock()
        .expect("supervision retry state is never poisoned")
        .pass_finished = Some(barrier);
    restarted
        .reconcile_workers()
        .await
        .expect("startup reconciliation");

    let calls = supervisor.calls();
    assert!(
        matches!(
            &calls[before_restart..],
            [
                crate::runtime::lifecycle::tests::Call::Inspect(_),
                crate::runtime::lifecycle::tests::Call::Retire(_),
            ]
        ),
        "the answering worker's job is inspected, then retired: {calls:?}"
    );
    assert_eq!(
        supervisor.retired(),
        std::slice::from_ref(&service_id),
        "the generation is retired before anything else is touched"
    );
    assert!(
        worktree.is_dir(),
        "an unconfirmed retirement keeps the checkout"
    );
    assert_eq!(daemon.bindings().len(), 1, "and its binding");
    assert_eq!(daemon.records().len(), 1, "and its record");
    let sessions = restarted.list().await;
    let [session] = sessions.as_slice() else {
        panic!("the create stays visible for the retry: {sessions:?}");
    };
    let runtime = session.runtime.as_ref().expect("runtime");
    assert_eq!(runtime.state, RuntimeState::Reconnecting);
    assert_eq!(
        runtime.loss_reason.as_deref(),
        Some(crate::runtime::lifecycle::SUPERVISION_UNAVAILABLE)
    );

    tokio::time::timeout(DETACHED_COMMIT_TIMEOUT, async {
        loop {
            let (pass, resume) = passes.recv().await.expect("retry barrier open");
            resume
                .send(())
                .expect("the retry loop waits at the barrier");
            if pass == created.id && daemon.records().is_empty() {
                return;
            }
        }
    })
    .await
    .expect("the supervision retry settles the ended create");

    assert_eq!(
        supervisor.retired(),
        [service_id.clone(), service_id],
        "the retry retires the exact generation again"
    );
    daemon
        .assert_compensated(&restarted, &worktree, "feat/ended")
        .await;
}

/// Arms `registry` to park its next committed create before its initial
/// input.
fn hold_initial_input(registry: &SessionRegistry) -> crate::runtime::lifecycle::tests::StartGate {
    let gate = crate::runtime::lifecycle::tests::StartGate {
        session_id: None,
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    };
    *registry
        .inner
        .initial_input_hold
        .lock()
        .expect("initial input hold is never poisoned") = Some(gate.clone());
    gate
}

/// The durable record of the only session, which is committed and still
/// marked as waiting for its initial input.
fn undelivered_create(daemon: &RestartableDaemon) -> crate::store::SessionRecord {
    let records = daemon.records();
    let [record] = records.as_slice() else {
        panic!("exactly one session record: {records:?}");
    };
    assert!(
        record
            .transaction
            .as_ref()
            .is_some_and(crate::store::SessionTransaction::is_initial_input),
        "the committed create is marked until its input is delivered: {record:?}"
    );
    assert!(
        record.runtime.generation.is_some(),
        "its runtime was launched"
    );
    record.clone()
}

#[tokio::test]
async fn daemon_shutdown_drains_a_create_through_its_initial_input() {
    let daemon = RestartableDaemon::new("wt-drain-input");
    let (registry, _supervisor) = scripted_registry(daemon.config.clone());
    let gate = hold_initial_input(&registry);
    let creating = tokio::spawn({
        let registry = registry.clone();
        let params = SessionNewParams {
            input: Some("echo drained".to_owned()),
            ..daemon.params("feat/drained-input")
        };
        async move { registry.create(params).await }
    });
    gate.entered.notified().await;
    let pending = undelivered_create(&daemon);

    registry.begin_daemon_shutdown();
    let mut drain = Box::pin(registry.drain_creates());
    assert!(
        futures::poll!(drain.as_mut()).is_pending(),
        "the drain waits for the initial input"
    );
    gate.release.notify_one();

    assert!(drain.await, "the drain ends once the input is delivered");
    let created = creating
        .await
        .expect("create task joins")
        .expect("the create delivers its input");
    let record = stored_record(&daemon.store_path, &created.id);
    assert_eq!(record.transaction, None, "delivery clears the marker");

    // A writer that built its record before the clearing cannot bring the
    // marker back.
    daemon
        .store()
        .record_session(&pending)
        .expect("write a stale marked record");
    assert_eq!(
        stored_record(&daemon.store_path, &created.id).transaction,
        None,
        "a delivered create is never marked again"
    );
}

#[tokio::test]
async fn create_cut_off_before_its_initial_input_is_rolled_back_at_restart() {
    let daemon = RestartableDaemon::new("wt-cut-input");
    let (registry, supervisor) = scripted_registry(SessionRegistryConfig {
        create_drain_timeout: Duration::ZERO,
        ..daemon.config.clone()
    });
    let gate = hold_initial_input(&registry);
    let _creating = tokio::spawn({
        let registry = registry.clone();
        let params = SessionNewParams {
            input: Some("echo lost".to_owned()),
            ..daemon.params("feat/cut-input")
        };
        async move { registry.create(params).await }
    });
    gate.entered.notified().await;

    registry.begin_daemon_shutdown();
    assert!(
        !registry.drain_creates().await,
        "the drain gives up before the input is delivered"
    );
    // The input exists only in memory, so the durable record says the create
    // is unfinished.
    let record = undelivered_create(&daemon);
    let worktree = record.info.worktree_path.clone().expect("bound worktree");
    let service_id = record.runtime.service_id.clone().expect("recorded job");
    // The daemon exits: its in-memory session and worker connections go
    // away, while the worker keeps running under its job.
    let entry = registry
        .inner
        .sessions
        .lock()
        .await
        .remove(&SessionId(record.session_id.clone()))
        .expect("committed session entry");
    entry.cancel_runtime_watchers();
    drop(entry);
    drop(registry);

    let restarted = SessionRegistry::new_with_launcher_and_inspector(
        daemon.config.clone(),
        Arc::clone(&supervisor) as Arc<dyn crate::runtime::WorkerLauncher>,
        Arc::new(ReadableHost::new()),
    );
    restarted
        .reconcile_workers()
        .await
        .expect("startup reconciliation");

    assert!(
        supervisor
            .retired()
            .iter()
            .any(|retired| retired.as_str() == service_id),
        "the running generation is retired: {:?}",
        supervisor.calls()
    );
    daemon
        .assert_compensated(&restarted, &worktree, "feat/cut-input")
        .await;
}

/// A create with an initial input parked between its delivered input and
/// the durable clearing of its `initial_input` marker.
struct InputCommitRace {
    daemon: RestartableDaemon,
    registry: SessionRegistry,
    supervisor: Arc<crate::runtime::lifecycle::tests::ScriptedSupervisor>,
    creating: tokio::task::JoinHandle<Result<SessionInfo, protocol::ProtocolError>>,
    release: Arc<tokio::sync::Notify>,
    id: SessionId,
}

impl InputCommitRace {
    async fn start(tag: &str, shell: ShellCommand) -> Self {
        let daemon = RestartableDaemon::new(tag);
        let (registry, supervisor) = scripted_registry(SessionRegistryConfig {
            shell_command: shell,
            initial_input_startup_grace: Duration::ZERO,
            ..daemon.config.clone()
        });
        let gate = crate::runtime::lifecycle::tests::StartGate {
            session_id: None,
            entered: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
        };
        *registry
            .inner
            .initial_input_commit_hold
            .lock()
            .expect("initial input commit hold is never poisoned") = Some(gate.clone());
        let creating = tokio::spawn({
            let registry = registry.clone();
            let params = SessionNewParams {
                input: Some("go".to_owned()),
                ..daemon.params(&format!("feat/{tag}"))
            };
            async move { registry.create(params).await }
        });
        gate.entered.notified().await;
        let id = SessionId(undelivered_create(&daemon).session_id);
        Self {
            daemon,
            registry,
            supervisor,
            creating,
            release: gate.release,
            id,
        }
    }

    /// Lets the marker commit run and returns the create's outcome.
    async fn commit(self) -> (RestartableDaemon, SessionRegistry, SessionId, SessionInfo) {
        self.release.notify_one();
        let created = self
            .creating
            .await
            .expect("create task joins")
            .expect("the delivered create succeeds");
        (self.daemon, self.registry, self.id, created)
    }
}

#[tokio::test]
async fn initial_input_commit_keeps_a_concurrent_stop() {
    let race = InputCommitRace::start(
        "input-commit-stop",
        ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
    )
    .await;
    race.registry.stop(&race.id).await.expect("stop session");

    let (daemon, _registry, id, _created) = race.commit().await;

    let record = stored_record(&daemon.store_path, &id);
    assert_eq!(record.transaction, None, "the marker is cleared");
    assert_eq!(
        record.desired_state,
        crate::store::DesiredState::Stopped,
        "the explicit stop survives the commit"
    );
    assert_eq!(record.info.state, SessionState::Stopped);
    assert_eq!(record.runtime.state, RuntimeState::Terminal);
}

#[tokio::test]
async fn initial_input_commit_keeps_a_concurrent_remove() {
    let race = InputCommitRace::start(
        "input-commit-remove",
        ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
    )
    .await;
    race.registry
        .remove(&race.id)
        .await
        .expect("remove session");

    let (daemon, registry, _id, _created) = race.commit().await;

    assert!(
        daemon.records().is_empty(),
        "the commit does not bring the removed session back"
    );
    assert!(registry.list().await.is_empty());
}

#[tokio::test]
async fn initial_input_commit_keeps_a_concurrent_natural_exit() {
    let race = InputCommitRace::start(
        "input-commit-exit",
        ShellCommand::new("/bin/sh", ["-c", "read line; exit 3"]),
    )
    .await;
    let exited = race
        .registry
        .wait_for_exit(&race.id, DETACHED_COMMIT_TIMEOUT)
        .await
        .expect("the delivered input ends the shell");
    assert_eq!(exited.exit_code, Some(3));

    let (daemon, _registry, id, _created) = race.commit().await;

    let record = stored_record(&daemon.store_path, &id);
    assert_eq!(record.transaction, None, "the marker is cleared");
    assert_eq!(record.info.state, SessionState::Failed, "the exit survives");
    assert_eq!(record.info.exit_code, Some(3));
    assert_eq!(record.runtime.state, RuntimeState::Terminal);
}

/// A create whose initial input was never delivered because its daemon
/// exited, and the replacement daemon that finds its `initial_input` marker.
struct OrphanedInputCreate {
    daemon: RestartableDaemon,
    supervisor: Arc<crate::runtime::lifecycle::tests::ScriptedSupervisor>,
    restarted: SessionRegistry,
    id: SessionId,
    worktree: PathBuf,
}

impl OrphanedInputCreate {
    async fn start(tag: &str) -> Self {
        let daemon = RestartableDaemon::new(tag);
        let (registry, supervisor) = scripted_registry(daemon.config.clone());
        let gate = hold_initial_input(&registry);
        let _creating = tokio::spawn({
            let registry = registry.clone();
            let params = SessionNewParams {
                input: Some("echo orphaned".to_owned()),
                ..daemon.params(&format!("feat/{tag}"))
            };
            async move { registry.create(params).await }
        });
        gate.entered.notified().await;
        let record = undelivered_create(&daemon);
        let id = SessionId(record.session_id.clone());
        // The daemon exits: its in-memory session and worker connections go
        // away, while the worker keeps running under its job.
        let entry = registry
            .inner
            .sessions
            .lock()
            .await
            .remove(&id)
            .expect("committed session entry");
        entry.cancel_runtime_watchers();
        drop(entry);
        drop(registry);
        let restarted = SessionRegistry::new_with_launcher_and_inspector(
            daemon.config.clone(),
            Arc::clone(&supervisor) as Arc<dyn crate::runtime::WorkerLauncher>,
            Arc::new(ReadableHost::new()),
        );
        Self {
            daemon,
            supervisor,
            restarted,
            id,
            worktree: record.info.worktree_path.clone().expect("bound worktree"),
        }
    }

    /// Asserts, with the session's lifecycle lock held so no retry can act,
    /// that nothing was stopped or cleaned and the session waits as pending.
    async fn assert_untouched_and_pending(&self) {
        let _guard = self.restarted.lock_lifecycle(&self.id).await;
        assert!(
            self.supervisor.retired().is_empty(),
            "no generation is retired: {:?}",
            self.supervisor.calls()
        );
        assert!(self.worktree.is_dir(), "the worktree is kept");
        assert_eq!(self.daemon.bindings().len(), 1, "its binding is kept");
        let record = undelivered_create(&self.daemon);
        assert_eq!(
            record.desired_state,
            crate::store::DesiredState::Running,
            "no removal intent was persisted"
        );
        let sessions = self.restarted.list().await;
        let [session] = sessions.as_slice() else {
            panic!("the session stays visible for the retry: {sessions:?}");
        };
        let runtime = session.runtime.as_ref().expect("runtime");
        assert_eq!(runtime.state, RuntimeState::Conflict);
        assert_eq!(
            runtime.loss_reason.as_deref(),
            Some(crate::runtime::lifecycle::SUPERVISION_AMBIGUOUS)
        );
    }
}

#[tokio::test]
async fn supervision_retry_never_rolls_back_a_live_create_committing_its_input() {
    let race = InputCommitRace::start(
        "input-commit-retry",
        ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
    )
    .await;
    // The worker connection is lost while the create commits its delivered
    // input, leaving the session reconnecting for the supervision retry.
    let mut sessions = race.registry.inner.sessions.lock().await;
    let entry = sessions.get_mut(&race.id).expect("session entry");
    entry.cancel_runtime_watchers();
    entry.runtime = RuntimeHandle::Unavailable(RuntimeState::Reconnecting);
    let runtime = entry.info.runtime.as_mut().expect("runtime");
    runtime.state = RuntimeState::Reconnecting;
    runtime.loss_reason = Some(crate::runtime::lifecycle::SUPERVISION_UNAVAILABLE.to_owned());
    drop(sessions);
    {
        let _guard = race.registry.lock_lifecycle(&race.id).await;
        let record = race
            .registry
            .load_durable_session_record(&race.id)
            .await
            .expect("read record")
            .expect("session record");
        assert!(
            record
                .transaction
                .as_ref()
                .is_some_and(crate::store::SessionTransaction::is_initial_input),
            "the retry sees this daemon's marker"
        );
        race.registry.reconcile_single_session(record).await
    };
    let supervisor = Arc::clone(&race.supervisor);

    let (daemon, registry, id, created) = race.commit().await;

    assert!(
        supervisor.retired().is_empty(),
        "the live create's generation is never retired: {:?}",
        supervisor.calls()
    );
    let record = stored_record(&daemon.store_path, &id);
    assert_eq!(record.transaction, None, "the delivered input is committed");
    assert_eq!(record.desired_state, crate::store::DesiredState::Running);
    let worktree = created.worktree_path.expect("bound worktree");
    assert!(worktree.is_dir(), "the live create keeps its worktree");
    assert_eq!(daemon.bindings().len(), 1);
    registry.stop(&id).await.expect("stop the session");
}

#[tokio::test]
async fn failed_undelivered_create_conversion_stops_and_cleans_nothing() {
    let orphan = OrphanedInputCreate::start("orphan-io").await;
    orphan
        .restarted
        .inner
        .store
        .as_ref()
        .expect("registry store")
        .fail_next_write_before_rename();

    orphan
        .restarted
        .reconcile_workers()
        .await
        .expect("startup reconciliation");

    orphan.assert_untouched_and_pending().await;
}

#[tokio::test]
async fn stale_undelivered_create_conversion_stops_and_cleans_nothing() {
    let orphan = OrphanedInputCreate::start("orphan-stale").await;
    let gate = crate::runtime::lifecycle::tests::StartGate {
        session_id: None,
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    };
    *orphan
        .restarted
        .inner
        .undelivered_conversion_hold
        .lock()
        .expect("conversion hold is never poisoned") = Some(gate.clone());
    let reconciling = tokio::spawn({
        let restarted = orphan.restarted.clone();
        async move { restarted.reconcile_workers().await }
    });
    gate.entered.notified().await;
    // Another writer persists a newer state of the record the conversion read.
    let mut newer = undelivered_create(&orphan.daemon);
    newer.info.updated_at = timestamp_now();
    newer.info.name = Some("renamed meanwhile".to_owned());
    orphan
        .daemon
        .store()
        .record_session(&newer)
        .expect("persist the newer record");
    gate.release.notify_one();

    reconciling
        .await
        .expect("reconciliation joins")
        .expect("startup reconciliation");

    orphan.assert_untouched_and_pending().await;
    assert_eq!(
        stored_record(&orphan.daemon.store_path, &orphan.id),
        newer,
        "the newer record is not overwritten"
    );
}

/// Stop grace of the commit-failure fixture: longer than
/// [`COMMIT_FAILURE_KILL_DELAY`], so the worker kill lands while the stop of
/// the half-launched runtime is still in flight.
const COMMIT_FAILURE_STOP_GRACE: Duration = Duration::from_secs(2);

/// Delay between the worker journal naming its runtime and the worker kill
/// in the commit-failure fixture. It exceeds the daemon's initialize,
/// inspect, and failed-commit path (milliseconds) and stays inside the
/// two-second stop grace, so the stop of the half-launched runtime is still
/// awaiting the trapping child when its worker connection drops.
const COMMIT_FAILURE_KILL_DELAY: Duration = Duration::from_millis(300);

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "the combined commit, stop, and retire sabotage keeps its setup visible end to end"
)]
async fn failed_commit_stop_and_retire_keep_the_session_reconnecting() {
    // Short, unique root: worker socket paths stay inside the `sun_path`
    // bound, like the reconciliation subprocess fixtures.
    let root = crate::test_support::thread_scoped_dir("cf-");
    let store_path = root.join("data/metadata.jsonl");
    let data_dir = root.join("data");
    let pid_file = root.join("child.pid");
    let script = format!(
        "echo $$ > {pid}; trap '' TERM HUP INT; while :; do sleep 1; done",
        pid = pid_file.display(),
    );
    let environment = crate::runtime::SubprocessWorkerEnvironment {
        runtime_home: root.join("r"),
        state_home: root.join("s"),
        data_home: root.join("d"),
        config_home: root.join("c"),
        cache_home: root.join("k"),
        home: root.clone(),
        daemon_socket: root.join("daemon.sock"),
    };
    let mut supervision = environment
        .supervision(pohunek_test_support::worker_binary())
        .with_environment_source(crate::test_support::thread_environment_source());
    supervision.sweep_grace = Duration::from_secs(5);
    let launcher = crate::runtime::SubprocessWorkerLauncher::new();
    let supervisor = Arc::new(crate::runtime::lifecycle::tests::ScriptedSupervisor::over(
        launcher.clone(),
    ));
    let registry = SessionRegistry::new_with_launcher_and_inspector(
        SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c".to_owned(), script]),
            store_path: Some(store_path.clone()),
            worker_runtime_root: Some(root.join("r/pohunek/workers")),
            worker_state_root: Some(root.join("s/pohunek/workers")),
            worker_connect_deadline: Duration::from_secs(10),
            supervision: Some(supervision),
            stop_grace: COMMIT_FAILURE_STOP_GRACE,
            ..SessionRegistryConfig::default()
        },
        Arc::clone(&supervisor) as Arc<dyn crate::runtime::WorkerLauncher>,
        Arc::new(ReadableHost::new()),
    );

    let gate = crate::runtime::lifecycle::tests::StartGate {
        session_id: None,
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    };
    supervisor.hold_start(gate.clone());

    let creating = tokio::spawn({
        let registry = registry.clone();
        async move { registry.create(params()).await }
    });
    gate.entered.notified().await;

    // The preparing record is durable, so the session and job identities are
    // fixed and every follow-up can be sabotaged by name.
    let record = {
        let records = crate::store::Store::new(store_path.clone())
            .load_sessions()
            .expect("load preparing record");
        let [record] = records.as_slice() else {
            panic!("one preparing record: {records:?}");
        };
        record.clone()
    };
    let session_id = SessionId(record.session_id.clone());
    let service_id = pohunek_platform::supervisor::ServiceId::parse(
        record
            .runtime
            .service_id
            .clone()
            .expect("the preparing record names its job"),
    )
    .expect("service id");
    // The final commit of the registration must fail...
    fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o500))
        .expect("read-only store directory");
    // ... the best-effort stop must hit a dying worker, and the retire must
    // be answered by an unavailable manager.
    supervisor.script_retire_unavailable(service_id.clone());
    let kill = tokio::spawn({
        let launcher = launcher.clone();
        let journal_dir = root.join("s/pohunek/workers").join(session_id.0.clone());
        let session = session_id.0.clone();
        async move {
            wait_until("the worker to journal its runtime", || async {
                std::fs::read_dir(&journal_dir).ok().and_then(|entries| {
                    entries.flatten().find_map(|entry| {
                        let journal: Option<serde_json::Value> =
                            std::fs::read_to_string(entry.path())
                                .ok()
                                .and_then(|text| serde_json::from_str(&text).ok());
                        let named = journal
                            .as_ref()
                            .and_then(|journal| journal.get("runtime_id"))
                            .is_some_and(serde_json::Value::is_string);
                        named.then_some(())
                    })
                })
            })
            .await;
            tokio::time::sleep(COMMIT_FAILURE_KILL_DELAY).await;
            let _ = launcher.kill_worker(&session).await;
        }
    });
    gate.release.notify_one();

    let error = guard("the create to finish", creating)
        .await
        .expect("create task joins")
        .expect_err("the registration cannot commit");
    kill.await.expect("kill task");

    assert_eq!(
        error.code,
        crate::runtime::lifecycle::SUPERVISION_UNAVAILABLE
    );
    assert_eq!(
        supervisor.retired(),
        vec![service_id],
        "the retire was attempted and refused"
    );
    let pending = registry
        .inner
        .supervision_retries
        .state
        .lock()
        .expect("supervision retry state is never poisoned")
        .pending
        .clone();
    assert!(
        pending.contains(&session_id.0),
        "an unproven retire schedules a supervision retry: {pending:?}"
    );
    // The store refuses non-private directories, so the fixture restores the
    // mode before reading back the record the branch kept.
    fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o700))
        .expect("restore store directory");
    assert_eq!(
        stored_record(&store_path, &session_id).runtime.generation,
        record.runtime.generation,
        "the preparing record is kept for reconciliation, not deleted"
    );

    // The worker was killed, so its trapping child is an orphan now.
    if let Ok(pid) = fs::read_to_string(&pid_file) {
        let _ = pohunek_test_support::process_env::command("kill")
            .args(["-KILL", pid.trim()])
            .status();
    }
    let _ = fs::remove_dir_all(&root);
}

async fn terminal_resumable_session(
    registry: &SessionRegistry,
    exit_gate: &mut fs::File,
) -> SessionInfo {
    let created = registry
        .create(resumable_params())
        .await
        .expect("create session");
    let recorded = registry
        .report_native_id(native_report!(registry;
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: format!("native-{}", created.id.0),
            transcript_path: None,
        ))
        .await;
    assert!(recorded.recorded, "native id captured");
    exit_gate
        .write_all(b"go\n")
        .expect("release the agent exit gate");
    let done = registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("session exits");
    assert_eq!(done.state, SessionState::Done);
    done
}

#[tokio::test]
async fn concurrent_recoveries_of_one_session_start_exactly_one_generation() {
    let marker = temp_dir("lifecycle-concurrent-resume-marker").join("argv.txt");
    let (agents_dir, mut exit_gate) =
        temp_agent_that_exits_then_resumes("lifecycle-concurrent-resume", &marker);
    let (registry, supervisor) = scripted_registry(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(temp_store_path("lifecycle-concurrent-resume")),
        agents_dir: Some(agents_dir),
        socket_path: Some(PathBuf::from("/run/pohunek/d.sock")),
        ..SessionRegistryConfig::default()
    });
    let done = terminal_resumable_session(&registry, &mut exit_gate).await;
    let [first_job] = supervisor.started().try_into().expect("one created job");

    let (left, right) = tokio::join!(registry.resume(&done.id), registry.resume(&done.id));

    let outcomes = [left, right];
    let recovered = outcomes
        .iter()
        .filter_map(|outcome| outcome.as_ref().ok())
        .collect::<Vec<_>>();
    assert_eq!(
        recovered.len(),
        1,
        "exactly one recovery wins: {outcomes:?}"
    );
    let refused = outcomes
        .iter()
        .find_map(|outcome| outcome.as_ref().err())
        .expect("the other recovery is refused");
    assert_eq!(refused.code, "session_runtime_not_recoverable");
    let started = supervisor.started();
    assert_eq!(started.len(), 2, "create plus one recovery: {started:?}");
    assert_eq!(
        supervisor.retired(),
        [first_job],
        "the superseded generation is retired exactly"
    );
    assert_eq!(supervisor.live_jobs().await, [started[1].clone()]);
    registry
        .stop(&done.id)
        .await
        .expect("stop recovered session");
}

#[tokio::test]
async fn recoveries_of_different_sessions_are_not_serialized() {
    let marker = temp_dir("lifecycle-parallel-resume-marker").join("argv.txt");
    let (agents_dir, mut exit_gate) =
        temp_agent_that_exits_then_resumes("lifecycle-parallel-resume", &marker);
    let (registry, supervisor) = scripted_registry(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(temp_store_path("lifecycle-parallel-resume")),
        agents_dir: Some(agents_dir),
        socket_path: Some(PathBuf::from("/run/pohunek/d.sock")),
        ..SessionRegistryConfig::default()
    });
    let held = terminal_resumable_session(&registry, &mut exit_gate).await;
    let free = terminal_resumable_session(&registry, &mut exit_gate).await;
    let gate = crate::runtime::lifecycle::tests::StartGate {
        session_id: Some(held.id.0.clone()),
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    };
    supervisor.hold_start(gate.clone());

    let held_recovery = tokio::spawn({
        let registry = registry.clone();
        let id = held.id.clone();
        async move { registry.resume(&id).await }
    });
    gate.entered.notified().await;
    let recovered = tokio::time::timeout(DETACHED_COMMIT_TIMEOUT, registry.resume(&free.id))
        .await
        .expect("another session recovers while the first is held")
        .expect("recover the free session");
    assert_eq!(recovered.id, free.id);

    gate.release.notify_one();
    held_recovery
        .await
        .expect("held recovery task")
        .expect("held recovery completes once released");
    registry.stop(&held.id).await.expect("stop held session");
    registry.stop(&free.id).await.expect("stop free session");
}

/// A Pi-shaped agent that appends `launch` and its arguments to `marker` on
/// every start. A start with `--session-id` stays alive until the returned gate
/// is written; every other start (resume, fork) stays alive for the whole test.
#[cfg(unix)]
pub(super) fn assigned_agent_script(
    dir: &std::path::Path,
    marker: &std::path::Path,
) -> (PathBuf, fs::File) {
    let script = dir.join("pi-like");
    let gate_path = dir.join("exit.gate");
    let gate = hook_gate(&gate_path);
    write_executable(
        &script,
        &format!(
            "#!/bin/sh\nprintf 'launch\\n' >> '{marker}'\nprintf '%s\\n' \"$@\" >> '{marker}'\ncase \" $* \" in *\" --session-id \"*) read _ < '{gate}'; exit 0 ;; *) sleep 30 ;; esac\n",
            marker = marker.display(),
            gate = gate_path.display(),
        ),
    );
    (script, gate)
}

/// Lines of the launch `index` (zero-based) recorded in `marker`.
#[cfg(unix)]
pub(super) fn recorded_launch(marker: &std::path::Path, index: usize) -> Vec<String> {
    fs::read_to_string(marker)
        .expect("read argv marker")
        .split("launch\n")
        .nth(index + 1)
        .map(|launch| launch.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

#[cfg(unix)]
fn assigned_registry(
    script: &std::path::Path,
    existence: &str,
    store_path: &std::path::Path,
    agents_dir: Option<PathBuf>,
) -> SessionRegistry {
    SessionRegistry::new_with_runtimes(
        SessionRegistryConfig {
            shell_command: hermetic_shell(),
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path.to_path_buf()),
            agents_dir,
            ..SessionRegistryConfig::default()
        },
        crate::agent::host::fixture::pi_shaped_host(script, existence),
    )
}

/// Asserts that the durable record and the resume store of `id` hold
/// `reference` with provenance `assigned`, frozen assignment and no
/// process-bound ordering key.
#[cfg(unix)]
fn assert_assigned_binding_persisted(store: &crate::store::Store, id: &SessionId, reference: &str) {
    let record = store
        .load_sessions()
        .expect("durable sessions")
        .into_iter()
        .find(|record| record.session_id == id.0)
        .expect("durable record");
    let recovery = record.recovery.expect("durable recovery binding");
    assert_eq!(recovery.native_session_id.as_deref(), Some(reference));
    assert_eq!(
        recovery.native_reference_provenance,
        crate::agent::NativeReferenceProvenance::Assigned
    );
    assert!(
        record.native_identity_ordering.is_none(),
        "an assigned reference is no identity evidence"
    );
    let legacy = store.load_resume().expect("resume store");
    let legacy = legacy
        .iter()
        .find(|binding| binding.session_id == id.0)
        .expect("resume binding is persisted at create");
    assert_eq!(
        legacy.native_reference_provenance,
        crate::agent::NativeReferenceProvenance::Assigned
    );
    let launch_argv = legacy
        .native_launch
        .as_ref()
        .and_then(NativeSessionLaunch::assigned)
        .map(|assigned| {
            assigned.launch_argv(&crate::agent::SessionRef::id(reference).expect("id reference"))
        });
    assert_eq!(
        launch_argv,
        Some(vec!["--session-id".to_owned(), reference.to_owned()]),
        "the assignment is frozen into the binding"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_hook_less_runtime_launches_with_an_assigned_reference_and_recovers_from_it() {
    let dir = temp_dir("assigned-e2e");
    let marker = dir.join("argv.txt");
    let (script, mut gate) = assigned_agent_script(&dir, &marker);
    let store_path = temp_store_path("assigned-e2e");
    let registry = assigned_registry(
        &script,
        crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
        &store_path,
        None,
    );

    let created = registry
        .create(SessionNewParams {
            agent: "pi".to_owned(),
            cwd: Some(dir.clone()),
            input: Some("hello".to_owned()),
            ..params()
        })
        .await
        .expect("create a hook-less package session");

    // Launch: fixed args, the assigned template, then the prompt.
    let first = wait_for_file_contains(&marker, "hello").await;
    assert!(first.starts_with("launch\n"));
    let launch = recorded_launch(&marker, 0);
    let reference = created
        .native_session_id
        .clone()
        .expect("the session holds the assigned reference at once");
    assert_eq!(
        launch,
        [
            "--model",
            "fast",
            "--session-id",
            reference.as_str(),
            "hello"
        ]
    );
    assert!(
        uuid::Uuid::parse_str(&reference).is_ok(),
        "core generates a UUID"
    );
    assert!(created.capabilities.resume && created.capabilities.fork);

    // Persistence: both the durable record and the resume store carry the
    // reference with its provenance, and no process-bound ordering exists.
    let store = crate::store::Store::new(store_path.clone());
    assert_assigned_binding_persisted(&store, &created.id, &reference);

    // Resume: the frozen args and the resume template, never the start flag.
    gate.write_all(b"go\n").expect("release the exit gate");
    registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("session exits");
    registry.resume(&created.id).await.expect("native recovery");
    wait_for_file_contains(&marker, "--session\n").await;
    assert_eq!(
        recorded_launch(&marker, 1),
        ["--model", "fast", "--session", reference.as_str()]
    );

    // Fork: the fork template with the source's reference; the child holds no
    // reference of its own because core cannot learn the forked conversation.
    let forked = registry
        .fork(SessionForkParams {
            session_id: created.id.clone(),
            name: None,
            cwd_mode: ForkCwdMode::Same,
            cols: 80,
            rows: 24,
            accept_profile_change: false,
        })
        .await
        .expect("native fork");
    wait_for_file_contains(&marker, "--fork\n").await;
    assert_eq!(
        recorded_launch(&marker, 2),
        ["--model", "fast", "--fork", reference.as_str()]
    );
    assert_eq!(forked.native_session_id, None);
    assert!(store
        .load_resume()
        .expect("resume store")
        .iter()
        .all(|binding| binding.session_id != forked.id.0));

    let _ = registry.stop(&forked.id).await;
    let _ = registry.stop(&created.id).await;
}

/// A Pi-shaped agent that answers `--version` from `version_file` and records
/// every other start in `marker`. A start with `--session-id` stays alive until
/// the returned gate is written; every other start stays alive for the whole
/// test.
#[cfg(unix)]
fn probed_agent_script(
    dir: &std::path::Path,
    marker: &std::path::Path,
    version_file: &std::path::Path,
) -> (PathBuf, fs::File) {
    let script = dir.join("pi-probed");
    let gate_path = dir.join("exit.gate");
    let gate = hook_gate(&gate_path);
    write_executable(
        &script,
        &format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then cat '{version}'; exit 0; fi\nprintf 'launch\\n' >> '{marker}'\nprintf '%s\\n' \"$@\" >> '{marker}'\ncase \" $* \" in *\" --session-id \"*) read _ < '{gate}'; exit 0 ;; *) sleep 30 ;; esac\n",
            version = version_file.display(),
            marker = marker.display(),
            gate = gate_path.display(),
        ),
    );
    (script, gate)
}

#[cfg(unix)]
#[tokio::test]
async fn a_probed_package_runtime_is_revalidated_before_every_launch_kind() {
    let dir = temp_dir("probed-launch");
    let marker = dir.join("argv.txt");
    let version_file = dir.join("version.txt");
    fs::write(&version_file, "1.0.2\n").expect("write the version");
    let (script, mut gate) = probed_agent_script(&dir, &marker, &version_file);
    let store_path = temp_store_path("probed-launch");
    let registry = SessionRegistry::new_with_runtimes(
        SessionRegistryConfig {
            shell_command: hermetic_shell(),
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path),
            ..SessionRegistryConfig::default()
        },
        crate::agent::host::fixture::pi_shaped_probed_host(
            &script,
            crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
            crate::agent::host::fixture::PI_SHAPED_PROBE,
        ),
    );
    let fork = |id: &SessionId| SessionForkParams {
        session_id: id.clone(),
        name: None,
        cwd_mode: ForkCwdMode::Same,
        cols: 80,
        rows: 24,
        accept_profile_change: false,
    };
    let launches = |marker: &std::path::Path| {
        fs::read_to_string(marker).map_or(0, |text| text.matches("launch\n").count())
    };

    // A release inside the range launches and forks.
    let created = registry
        .create(SessionNewParams {
            agent: "pi".to_owned(),
            cwd: Some(dir.clone()),
            ..params()
        })
        .await
        .expect("a release in the range launches");
    wait_for_file_contains(&marker, "--session-id\n").await;
    let forked = registry
        .fork(fork(&created.id))
        .await
        .expect("a release in the range forks");
    wait_for_file_contains(&marker, "--fork\n").await;
    assert_eq!(launches(&marker), 2);

    // A release outside the range is refused before anything starts.
    fs::write(&version_file, "1.1.0\n").expect("write the newer version");
    let refused = registry
        .fork(fork(&created.id))
        .await
        .expect_err("fork re-runs the probe");
    assert_eq!(refused.code, "agent_runtime_unsupported");
    assert_eq!(launches(&marker), 2, "nothing was launched");
    let refused = registry
        .create(SessionNewParams {
            agent: "pi".to_owned(),
            cwd: Some(dir.clone()),
            ..params()
        })
        .await
        .expect_err("create re-runs the probe");
    assert_eq!(refused.code, "agent_runtime_unsupported");

    gate.write_all(b"go\n").expect("release the exit gate");
    registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("the source session exits");
    let refused = registry
        .resume(&created.id)
        .await
        .expect_err("resume re-runs the probe");
    assert_eq!(refused.code, "agent_runtime_unsupported");
    assert_eq!(launches(&marker), 2, "nothing was launched");
    let _ = registry.stop(&forked.id).await;
}

/// The durable recovery binding of `id`, as a restarted daemon reads it.
#[cfg(unix)]
pub(super) fn durable_recovery(
    store_path: &std::path::Path,
    id: &SessionId,
) -> crate::store::ResumeBinding {
    crate::store::Store::new(store_path.to_path_buf())
        .load_sessions()
        .expect("durable sessions")
        .into_iter()
        .find(|record| record.session_id == id.0)
        .and_then(|record| record.recovery)
        .expect("durable recovery binding")
}

#[cfg(unix)]
#[tokio::test]
async fn recovery_refuses_an_assigned_reference_whose_conversation_is_missing() {
    let dir = temp_dir("assigned-existence");
    let home = temp_dir("assigned-existence-home");
    let marker = dir.join("argv.txt");
    let (script, mut gate) = assigned_agent_script(&dir, &marker);
    let store_path = temp_store_path("assigned-existence");
    // The profile points the runtime's config home at a test directory, so the
    // check never reads the host's environment.
    let agents_dir = temp_agents_dir_with(
        "assigned-existence",
        "pi-home",
        &format!(
            "base = \"pi\"\n{}program = \"{}\"\n[env]\n{} = \"{}\"\n",
            crate::agent::host::fixture::pi_shaped_profile_pin(),
            script.display(),
            crate::agent::host::fixture::PI_SHAPED_HOME_ENV,
            home.display()
        ),
    );
    let registry = assigned_registry(
        &script,
        crate::agent::host::fixture::PI_SHAPED_FILE_CHECK,
        &store_path,
        Some(agents_dir),
    );

    let created = registry
        .create(SessionNewParams {
            agent: "pi-home".to_owned(),
            cwd: Some(dir.clone()),
            ..params()
        })
        .await
        .expect("create a hook-less package session");
    let reference = created
        .native_session_id
        .clone()
        .expect("assigned reference");
    wait_for_file_contains(&marker, "--session-id").await;
    assert_eq!(
        recorded_launch(&marker, 0),
        ["--session-id", reference.as_str()],
        "a profile launches with its own args and the assigned template"
    );

    // The agent never wrote the conversation: a live fork is refused.
    let fork = SessionForkParams {
        session_id: created.id.clone(),
        name: None,
        cwd_mode: ForkCwdMode::Same,
        cols: 80,
        rows: 24,
        accept_profile_change: false,
    };
    let refused = registry
        .fork(fork.clone())
        .await
        .expect_err("fork needs the conversation");
    assert_eq!(refused.code, "agent_native_reference_missing");

    gate.write_all(b"go\n").expect("release the exit gate");
    registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("session exits");
    let refused = registry
        .resume(&created.id)
        .await
        .expect_err("recovery needs the conversation");
    assert_eq!(refused.code, "agent_native_reference_missing");
    assert_eq!(
        fs::read_to_string(&marker)
            .expect("marker")
            .matches("launch\n")
            .count(),
        1,
        "no agent is launched into an empty conversation"
    );
    let terminal = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(terminal.state, SessionState::Done);

    // The persisted binding a restarted daemon recovers from carries the
    // provenance, so the check also guards that path.
    let persisted = durable_recovery(&store_path, &created.id);
    assert_eq!(
        persisted.native_reference_provenance,
        crate::agent::NativeReferenceProvenance::Assigned
    );
    let refused = registry
        .resume_binding(persisted)
        .await
        .expect_err("a persisted binding is checked too");
    assert_eq!(refused.code, "agent_native_reference_missing");

    // Once the conversation exists, the same recovery goes through.
    let project = home.join("sessions").join("proj");
    fs::create_dir_all(&project).expect("session store dir");
    fs::write(
        project.join(format!("2026-10-04T10-00-00Z_{reference}.jsonl")),
        "{}",
    )
    .expect("conversation file");
    registry.resume(&created.id).await.expect("native recovery");
    wait_for_file_contains(&marker, "--session\n").await;
    assert_eq!(
        recorded_launch(&marker, 1),
        ["--session", reference.as_str()]
    );

    let _ = registry.stop(&created.id).await;
}

#[cfg(unix)]
#[tokio::test]
async fn a_report_labels_the_reference_it_writes_reported() {
    let dir = temp_dir("assigned-reported");
    let marker = dir.join("argv.txt");
    let (script, _gate) = assigned_agent_script(&dir, &marker);
    let store_path = temp_store_path("assigned-reported");
    let registry = SessionRegistry::new_with_runtimes(
        SessionRegistryConfig {
            shell_command: hermetic_shell(),
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path.clone()),
            ..SessionRegistryConfig::default()
        },
        crate::agent::host::fixture::pi_shaped_hooked_host(
            &script,
            crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
        ),
    );
    let created = registry
        .create(SessionNewParams {
            agent: "pi".to_owned(),
            cwd: Some(dir.clone()),
            ..params()
        })
        .await
        .expect("create session");
    assert!(
        registry
            .report_native_id(native_report!(&registry;
                session_id: created.id.clone(),
                agent: "pi".to_owned(),
                native_session_id: "reported-conversation".to_owned(),
                transcript_path: None,
            ))
            .await
            .recorded
    );

    let record = crate::store::Store::new(store_path)
        .load_sessions()
        .expect("durable sessions")
        .into_iter()
        .find(|record| record.session_id == created.id.0)
        .expect("durable record");
    let recovery = record.recovery.expect("recovery");
    assert_eq!(
        recovery.native_session_id.as_deref(),
        Some("reported-conversation")
    );
    assert_eq!(
        recovery.native_reference_provenance,
        crate::agent::NativeReferenceProvenance::Reported,
        "a reference written by a report is never labelled assigned"
    );
    assert!(record.native_identity_ordering.is_some());

    let _ = registry.stop(&created.id).await;
}

/// A fixed base-environment source: the thread fixture's variables plus
/// `extra`, with `HOME` set to `home`.
#[cfg(unix)]
fn assigned_environment(
    home: &std::path::Path,
    extra: &[(&str, &std::path::Path)],
) -> crate::runtime::EnvironmentSource {
    let crate::runtime::EnvironmentSource::Fixed(mut variables) =
        crate::test_support::thread_environment_source()
    else {
        panic!("a test fixture supplies an explicit environment");
    };
    variables.remove(std::ffi::OsStr::new("XDG_CONFIG_HOME"));
    variables.insert("HOME".into(), home.into());
    for (name, value) in extra {
        variables.insert((*name).into(), (*value).into());
    }
    crate::runtime::EnvironmentSource::Fixed(variables)
}

/// Writes `<root>/sessions/proj/<stamp>_<reference>.jsonl`.
#[cfg(unix)]
fn write_conversation(root: &std::path::Path, reference: &str) {
    let project = root.join("sessions").join("proj");
    fs::create_dir_all(&project).expect("session store dir");
    fs::write(
        project.join(format!("2026-10-04T10-00-00Z_{reference}.jsonl")),
        "{}",
    )
    .expect("conversation file");
}

/// Creates an assigned session of agent `agent`, lets it exit and returns its
/// reference with the recovery outcome. The registry's base environment is
/// `source` filtered by `allowlist`.
#[cfg(unix)]
async fn recover_assigned(
    tag: &str,
    agents_dir: Option<PathBuf>,
    source: crate::runtime::EnvironmentSource,
    allowlist: &[&str],
    prepare: impl FnOnce(&str),
) -> Result<(), String> {
    let dir = temp_dir(tag);
    let marker = dir.join("argv.txt");
    let (script, mut gate) = assigned_agent_script(&dir, &marker);
    if let Some(template) = &agents_dir {
        let body = fs::read_to_string(template.join("pi-env.toml"))
            .expect("profile template")
            .replace("SCRIPT", &script.display().to_string());
        fs::write(template.join("pi-env.toml"), body).expect("profile");
    }
    let registry = SessionRegistry::new_with_runtimes_and_environment(
        SessionRegistryConfig {
            shell_command: hermetic_shell(),
            stop_grace: Duration::from_millis(50),
            agents_dir,
            ..SessionRegistryConfig::default()
        },
        crate::agent::host::fixture::pi_shaped_host(
            &script,
            crate::agent::host::fixture::PI_SHAPED_XDG_CHECK,
        ),
        source,
        allowlist.iter().map(|name| (*name).to_owned()).collect(),
    );
    let agent = if registry.inner.profiles.resolve_agent("pi-env").is_ok() {
        "pi-env"
    } else {
        "pi"
    };
    let created = registry
        .create(SessionNewParams {
            agent: agent.to_owned(),
            cwd: Some(dir.clone()),
            ..params()
        })
        .await
        .expect("create session");
    let reference = created
        .native_session_id
        .clone()
        .expect("assigned reference");
    prepare(&reference);
    gate.write_all(b"go\n").expect("release the exit gate");
    registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("session exits");
    let outcome = registry.resume(&created.id).await.map(|_| ());
    let _ = registry.stop(&created.id).await;
    outcome.map_err(|error| error.code)
}

#[cfg(unix)]
#[tokio::test]
async fn recovery_reads_a_config_home_forwarded_through_the_base_environment() {
    let home = temp_dir("assigned-env-home");
    let xdg = temp_dir("assigned-env-xdg");
    let source = assigned_environment(&home, &[("XDG_CONFIG_HOME", &xdg)]);
    let outcome = recover_assigned(
        "assigned-env-forwarded",
        None,
        source,
        pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST,
        |reference| write_conversation(&xdg, reference),
    )
    .await;
    assert_eq!(
        outcome,
        Ok(()),
        "the forwarded XDG_CONFIG_HOME is the store"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_profile_variable_wins_over_the_base_environment_for_recovery() {
    let home = temp_dir("assigned-env-profile-home");
    let base_xdg = temp_dir("assigned-env-profile-base");
    let profile_xdg = temp_dir("assigned-env-profile-own");
    let agents_dir = temp_agents_dir_with(
        "assigned-env-profile",
        "pi-env",
        &format!(
            "base = \"pi\"\n{}program = \"SCRIPT\"\n[env]\nXDG_CONFIG_HOME = \"{}\"\n",
            crate::agent::host::fixture::pi_shaped_profile_pin(),
            profile_xdg.display()
        ),
    );
    let source = assigned_environment(&home, &[("XDG_CONFIG_HOME", &base_xdg)]);
    let outcome = recover_assigned(
        "assigned-env-profile-run",
        Some(agents_dir),
        source,
        pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST,
        |reference| write_conversation(&profile_xdg, reference),
    )
    .await;
    assert_eq!(outcome, Ok(()), "the profile's XDG_CONFIG_HOME wins");
}

#[cfg(unix)]
#[tokio::test]
async fn recovery_falls_back_to_the_declared_home_relative_store() {
    let home = temp_dir("assigned-env-fallback-home");
    let source = assigned_environment(&home, &[]);
    let outcome = recover_assigned(
        "assigned-env-fallback",
        None,
        source,
        pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST,
        |reference| write_conversation(&home.join(".config").join("pi"), reference),
    )
    .await;
    assert_eq!(outcome, Ok(()));
}

#[cfg(unix)]
#[tokio::test]
async fn a_home_the_allowlist_does_not_forward_is_not_used_for_recovery() {
    let spoofed_home = temp_dir("assigned-env-spoofed-home");
    let source = assigned_environment(&spoofed_home, &[]);
    let outcome = recover_assigned(
        "assigned-env-spoofed",
        None,
        source,
        &["PATH"],
        |reference| write_conversation(&spoofed_home.join(".config").join("pi"), reference),
    )
    .await;
    assert_eq!(
        outcome,
        Err("agent_native_reference_missing".to_owned()),
        "the agent would not see this HOME, so it is not the store"
    );
}

/// A Pi-shaped runtime installed as a real package, with the host that
/// serves it.
#[cfg(unix)]
struct PackagedPi {
    plugins: PathBuf,
    digest: package::PackageDigest,
    host: crate::agent::host::RuntimeHost,
}

#[cfg(unix)]
impl PackagedPi {
    /// Installs the package (enabled and selected) into a fresh plugin root
    /// and builds the host over it.
    fn install(tag: &str, script: &std::path::Path) -> Self {
        let plugins = temp_dir(tag).join("plugins");
        let (host, digest) = crate::agent::host::fixture::installed_pi_host(&plugins, script);
        Self {
            plugins,
            digest,
            host,
        }
    }

    /// [`Self::install`] for a package that declares an integration, whose
    /// sessions therefore admit hook reports.
    fn install_hooked(tag: &str, script: &std::path::Path) -> Self {
        let plugins = temp_dir(tag).join("plugins");
        let (host, digest) =
            crate::agent::host::fixture::installed_hooked_pi_host(&plugins, script);
        Self {
            plugins,
            digest,
            host,
        }
    }

    fn registry(&self, store_path: &std::path::Path) -> SessionRegistry {
        self.registry_with_agents(store_path, None)
    }

    fn registry_with_agents(
        &self,
        store_path: &std::path::Path,
        agents_dir: Option<PathBuf>,
    ) -> SessionRegistry {
        SessionRegistry::new_with_runtimes(
            SessionRegistryConfig {
                shell_command: hermetic_shell(),
                stop_grace: Duration::from_millis(50),
                store_path: Some(store_path.to_path_buf()),
                agents_dir,
                ..SessionRegistryConfig::default()
            },
            self.host.clone(),
        )
    }

    /// Disables the package and reloads the registry, as the lifecycle
    /// commands do.
    fn disable_and_reload(&self, registry: &SessionRegistry) {
        self.registry_handle()
            .set_enabled(&self.digest, false)
            .expect("disable");
        registry.reload_runtimes().expect("reload");
    }

    fn registry_handle(&self) -> package::registry::Registry {
        package::registry::Registry::open_at(&self.plugins, package::Limits::DEFAULT)
            .expect("registry")
    }

    /// Rewrites the installed descriptor with other bytes of the same length.
    fn tamper(&self) {
        let hex = self
            .digest
            .as_str()
            .strip_prefix("sha256:")
            .expect("digest prefix");
        let descriptor = self
            .plugins
            .join("packages")
            .join(hex)
            .join("files")
            .join("runtime.toml");
        let mut bytes = fs::read(&descriptor).expect("read descriptor");
        let at = bytes
            .windows(b"fast".len())
            .position(|window| window == b"fast")
            .expect("model argument");
        bytes[at..at + 4].copy_from_slice(b"slow");
        fs::write(&descriptor, bytes).expect("write descriptor");
    }
}

#[cfg(unix)]
fn packaged_params(dir: &std::path::Path) -> SessionNewParams {
    SessionNewParams {
        agent: "pi".to_owned(),
        cwd: Some(dir.to_path_buf()),
        ..params()
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_package_session_is_pinned_to_its_archive_digest() {
    let dir = temp_dir("packaged-pin");
    let marker = dir.join("argv.txt");
    let (script, _gate) = assigned_agent_script(&dir, &marker);
    let packaged = PackagedPi::install("packaged-pin-plugins", &script);
    let store_path = temp_store_path("packaged-pin");
    let registry = packaged.registry(&store_path);

    let created = registry
        .create(packaged_params(&dir))
        .await
        .expect("create a package session");

    let pin = durable_recovery(&store_path, &created.id).launch_binding;
    let Some(binding) = pin.binding() else {
        panic!("the session records a launch binding");
    };
    assert!(matches!(
        &binding.provenance,
        protocol::BindingProvenance::Package { package_digest, .. }
            if *package_digest == packaged.digest
    ));
    let _ = registry.stop(&created.id).await;
}

#[cfg(unix)]
#[tokio::test]
async fn a_package_modified_after_install_blocks_a_fresh_launch_and_leaves_no_record() {
    let dir = temp_dir("packaged-tamper-launch");
    let marker = dir.join("argv.txt");
    let (script, _gate) = assigned_agent_script(&dir, &marker);
    let packaged = PackagedPi::install("packaged-tamper-launch-plugins", &script);
    let store_path = temp_store_path("packaged-tamper-launch");
    let registry = packaged.registry(&store_path);
    let first = registry
        .create(packaged_params(&dir))
        .await
        .expect("an intact package launches");

    packaged.tamper();
    let refused = registry
        .create(packaged_params(&dir))
        .await
        .expect_err("a modified package must not launch");

    assert_eq!(refused.code, "runtime_incompatible");
    let records = crate::store::Store::new(store_path)
        .load_sessions()
        .expect("durable sessions");
    assert_eq!(
        records
            .iter()
            .map(|record| record.session_id.as_str())
            .collect::<Vec<_>>(),
        [first.id.0.as_str()],
        "the refused launch persisted nothing"
    );
    assert_eq!(registry.list().await.len(), 1);
    let _ = registry.stop(&first.id).await;
}

#[cfg(unix)]
#[tokio::test]
async fn resume_of_a_session_whose_package_was_modified_is_incompatible_and_keeps_its_binding() {
    let dir = temp_dir("packaged-tamper-resume");
    let marker = dir.join("argv.txt");
    let (script, mut gate) = assigned_agent_script(&dir, &marker);
    let packaged = PackagedPi::install("packaged-tamper-resume-plugins", &script);
    let store_path = temp_store_path("packaged-tamper-resume");
    let registry = packaged.registry(&store_path);
    let created = registry
        .create(packaged_params(&dir))
        .await
        .expect("create a package session");
    gate.write_all(b"go\n").expect("release the exit gate");
    registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("session exits");
    let before = durable_recovery(&store_path, &created.id);

    packaged.tamper();
    let refused = registry
        .resume(&created.id)
        .await
        .expect_err("a modified package must not resume");

    assert_eq!(refused.code, "runtime_incompatible");
    assert_eq!(
        durable_recovery(&store_path, &created.id).launch_binding,
        before.launch_binding,
        "the pin is left untouched, no other runtime takes over"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_session_resumes_from_its_pin_after_the_package_is_disabled() {
    let dir = temp_dir("packaged-disabled-resume");
    let marker = dir.join("argv.txt");
    let (script, mut gate) = assigned_agent_script(&dir, &marker);
    let packaged = PackagedPi::install("packaged-disabled-resume-plugins", &script);
    let store_path = temp_store_path("packaged-disabled-resume");
    let registry = packaged.registry(&store_path);
    let created = registry
        .create(packaged_params(&dir))
        .await
        .expect("create a package session");
    gate.write_all(b"go\n").expect("release the exit gate");
    registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("session exits");

    packaged
        .registry_handle()
        .set_enabled(&packaged.digest, false)
        .expect("disable");
    registry.reload_runtimes().expect("reload");

    let fresh = registry
        .create(packaged_params(&dir))
        .await
        .expect_err("a disabled package serves no fresh launch");
    assert_eq!(fresh.code, "agent_profile_not_found");
    registry
        .resume(&created.id)
        .await
        .expect("the pin keeps resuming from its own digest");
    let _ = registry.stop(&created.id).await;
}

#[cfg(unix)]
#[tokio::test]
async fn uninstall_is_refused_while_a_live_session_references_the_package() {
    let dir = temp_dir("packaged-uninstall");
    let marker = dir.join("argv.txt");
    let (script, _gate) = assigned_agent_script(&dir, &marker);
    let packaged = PackagedPi::install("packaged-uninstall-plugins", &script);
    let store_path = temp_store_path("packaged-uninstall");
    let registry = packaged.registry(&store_path);
    let created = registry
        .create(packaged_params(&dir))
        .await
        .expect("create a package session");

    let retained = registry.retained_package_digests().await.expect("retained");
    assert!(retained.contains(&packaged.digest));
    let refused = registry
        .uninstall_package(&packaged.digest)
        .await
        .expect_err("a referenced package cannot be uninstalled");
    assert!(matches!(
        refused,
        super::packages::PackageUninstallError::Registry(
            package::registry::RegistryError::StillReferenced
        )
    ));
    packaged
        .host
        .resolve_id(&protocol::RuntimeId::parse("pi").expect("id"))
        .expect("the runtime still serves");

    let _ = registry.stop(&created.id).await;
    registry.remove(&created.id).await.expect("remove");
    registry
        .uninstall_package(&packaged.digest)
        .await
        .expect("an unreferenced package uninstalls");
    assert!(registry
        .retained_package_digests()
        .await
        .expect("retained")
        .iter()
        .next()
        .is_none());
    assert_eq!(
        packaged
            .host
            .resolve_id(&protocol::RuntimeId::parse("pi").expect("id"))
            .expect_err("uninstalled and reloaded")
            .code,
        "runtime_not_installed"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn durable_bindings_without_a_live_session_still_retain_the_package() {
    let dir = temp_dir("packaged-retained-store");
    let marker = dir.join("argv.txt");
    let (script, mut gate) = assigned_agent_script(&dir, &marker);
    let packaged = PackagedPi::install("packaged-retained-store-plugins", &script);
    let store_path = temp_store_path("packaged-retained-store");
    let first = packaged.registry(&store_path);
    let created = first
        .create(packaged_params(&dir))
        .await
        .expect("create a package session");
    gate.write_all(b"go\n").expect("release the exit gate");
    first
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("session exits");
    drop(first);

    // A registry that holds no session in memory reads the same durable store.
    let second = packaged.registry(&store_path);
    let retained = second.retained_package_digests().await.expect("retained");

    assert!(retained.contains(&packaged.digest));
    assert!(second.uninstall_package(&packaged.digest).await.is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn a_registry_without_a_package_store_cannot_uninstall() {
    let registry = SessionRegistry::default();
    let digest = package::PackageDigest::parse(
        "sha256:5555555555555555555555555555555555555555555555555555555555555555",
    )
    .expect("digest");

    let error = registry
        .uninstall_package(&digest)
        .await
        .expect_err("no store");

    assert!(matches!(
        error,
        super::packages::PackageUninstallError::NoPackageStore
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn a_running_package_session_stays_mutable_when_its_active_agent_is_its_own_runtime() {
    let dir = temp_dir("packaged-active-agent");
    let marker = dir.join("argv.txt");
    let (script, _gate) = assigned_agent_script(&dir, &marker);
    let packaged = PackagedPi::install("packaged-active-agent-plugins", &script);
    let registry = packaged.registry(&temp_store_path("packaged-active-agent"));
    let created = registry
        .create(packaged_params(&dir))
        .await
        .expect("create a package session");
    registry
        .inner
        .sessions
        .lock()
        .await
        .get_mut(&created.id)
        .expect("session")
        .info
        .active_agent_base = Some(RuntimeRef::from_wire("pi"));

    packaged.disable_and_reload(&registry);

    registry
        .resize(&created.id, 100, 30)
        .await
        .expect("a running session of an installed package stays mutable");
    registry.stop(&created.id).await.expect("and stoppable");
    registry.remove(&created.id).await.expect("and removable");
}

#[cfg(unix)]
#[tokio::test]
async fn a_fork_holds_the_package_authority_until_its_registration_ends() {
    let dir = temp_dir("packaged-fork-guard");
    let marker = dir.join("argv.txt");
    let (script, _gate) = assigned_agent_script(&dir, &marker);
    let packaged = PackagedPi::install("packaged-fork-guard-plugins", &script);
    let registry = packaged.registry(&temp_store_path("packaged-fork-guard"));
    let created = registry
        .create(packaged_params(&dir))
        .await
        .expect("create a package session");
    packaged.disable_and_reload(&registry);

    // An uninstall in progress holds the exclusive authority.
    let exclusive = registry.inner.package_lifecycle.write().await;
    let forking = registry.clone();
    let source = created.id.clone();
    let mut fork = tokio::spawn(async move {
        forking
            .fork(SessionForkParams {
                session_id: source,
                name: None,
                cwd_mode: ForkCwdMode::Same,
                cols: 80,
                rows: 24,
                accept_profile_change: false,
            })
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut fork)
            .await
            .is_err(),
        "a fork waits for the package authority"
    );
    drop(exclusive);
    let forked = fork
        .await
        .expect("fork task")
        .expect("a pinned disabled package still forks");
    let _ = registry.stop(&forked.id).await;
    let _ = registry.stop(&created.id).await;
}

#[cfg(unix)]
#[tokio::test]
async fn an_uninstall_survives_the_cancellation_of_its_caller() {
    let dir = temp_dir("packaged-uninstall-cancel");
    let marker = dir.join("argv.txt");
    let (script, _gate) = assigned_agent_script(&dir, &marker);
    let packaged = PackagedPi::install("packaged-uninstall-cancel-plugins", &script);
    let registry = packaged.registry(&temp_store_path("packaged-uninstall-cancel"));

    // A launch in progress holds the shared authority.
    let launch = registry.inner.package_lifecycle.read().await;
    let caller = registry.clone();
    let digest = packaged.digest.clone();
    let outer = tokio::spawn(async move { caller.uninstall_package(&digest).await });
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    outer.abort();
    let _ = outer.await;
    drop(launch);
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }

    // The exclusive authority is free only once the uninstall transaction is
    // over, whoever awaited it.
    drop(registry.inner.package_lifecycle.write().await);
    assert!(
        packaged
            .registry_handle()
            .state()
            .expect("state")
            .packages()
            .is_empty(),
        "the transaction ran to completion without its caller"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn an_unreadable_store_record_blocks_uninstall_instead_of_vanishing_from_retention() {
    let dir = temp_dir("packaged-strict-retention");
    let marker = dir.join("argv.txt");
    let (script, _gate) = assigned_agent_script(&dir, &marker);
    let packaged = PackagedPi::install("packaged-strict-retention-plugins", &script);
    let store_path = temp_store_path("packaged-strict-retention");
    let registry = packaged.registry(&store_path);
    let created = registry
        .create(packaged_params(&dir))
        .await
        .expect("create a package session");
    let _ = registry.stop(&created.id).await;
    registry.remove(&created.id).await.expect("remove");
    let mut store = fs::OpenOptions::new()
        .append(true)
        .open(&store_path)
        .expect("open the store");
    std::io::Write::write_all(&mut store, b"{\"type\":\"resume\",\"damaged\":\n")
        .expect("append a damaged line");
    drop(store);

    let refused = registry
        .uninstall_package(&packaged.digest)
        .await
        .expect_err("a record that cannot be read may pin the package");

    assert!(matches!(
        refused,
        super::packages::PackageUninstallError::Retention(_)
    ));
    assert_eq!(
        packaged
            .registry_handle()
            .state()
            .expect("state")
            .packages()
            .len(),
        1
    );
}

#[cfg(unix)]
#[tokio::test]
async fn recovery_keeps_an_unchanged_profile_after_its_package_is_disabled() {
    let dir = temp_dir("packaged-profile-recovery");
    let marker = dir.join("argv.txt");
    let (script, mut gate) = assigned_agent_script(&dir, &marker);
    let packaged = PackagedPi::install("packaged-profile-recovery-plugins", &script);
    let agents_dir = temp_agents_dir_with(
        "packaged-profile-recovery",
        "pi-profile",
        &format!(
            "base = \"pi\"\npackage = \"{}\"\ndigest = \"{}\"\n\n[env]\nPROFILE_MARK = \"kept\"\n",
            crate::agent::host::fixture::PI_SHAPED_PACKAGE_ID,
            packaged.digest
        ),
    );
    let store_path = temp_store_path("packaged-profile-recovery");
    let registry = packaged.registry_with_agents(&store_path, Some(agents_dir));
    let created = registry
        .create(SessionNewParams {
            agent: "pi-profile".to_owned(),
            ..packaged_params(&dir)
        })
        .await
        .expect("create a profile session over the package");
    gate.write_all(b"go\n").expect("release the exit gate");
    registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("session exits");
    let binding = durable_recovery(&store_path, &created.id);

    packaged.disable_and_reload(&registry);
    let recovered = registry
        .resolve_recovery_profile(&binding, super::ProfileChange::Refuse)
        .expect("the unchanged profile still matches its frozen revision");

    assert_eq!(
        recovered.env,
        [("PROFILE_MARK".to_owned(), "kept".to_owned())],
        "the unchanged profile resolves against the pinned package"
    );
    assert!(
        recovered.revision.is_some() && recovered.revision == binding.profile_revision,
        "the pinned resolution reproduces the revision frozen at creation"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_resumed_package_session_detects_with_its_pinned_manifest_after_disable() {
    let dir = temp_dir("packaged-detector");
    let marker = dir.join("argv.txt");
    let (script, mut gate) = assigned_agent_script(&dir, &marker);
    let packaged = PackagedPi::install("packaged-detector-plugins", &script);
    let registry = packaged.registry(&temp_store_path("packaged-detector"));
    let created = registry
        .create(packaged_params(&dir))
        .await
        .expect("create a package session");
    let pinned = packaged
        .host
        .resolve_id(&protocol::RuntimeId::parse("pi").expect("id"))
        .expect("pinned definition");
    let expected = format!(
        "{:?}",
        crate::detect::DetectorConfig::for_definition(&pinned)
            .manifest
            .expect("manifest")
            .required_regions()
    );
    gate.write_all(b"go\n").expect("release the exit gate");
    registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("session exits");
    packaged.disable_and_reload(&registry);

    registry.resume(&created.id).await.expect("resume");

    let sessions = registry.inner.sessions.lock().await;
    let entry = sessions.get(&created.id).expect("session");
    let actual = format!(
        "{:?}",
        entry
            .detector_config
            .borrow()
            .config
            .manifest
            .as_ref()
            .expect("manifest")
            .required_regions()
    );
    drop(sessions);
    assert_eq!(actual, expected, "the pinned package's rules are in effect");
    let _ = registry.stop(&created.id).await;
}

/// Debug text of the regions a session's current detector manifest reads.
#[cfg(unix)]
async fn detector_regions(registry: &SessionRegistry, id: &SessionId) -> String {
    let sessions = registry.inner.sessions.lock().await;
    let entry = sessions.get(id).expect("session");
    let regions = format!(
        "{:?}",
        entry
            .detector_config
            .borrow()
            .config
            .manifest
            .as_ref()
            .expect("manifest")
            .required_regions()
    );
    regions
}

#[cfg(unix)]
fn pi_report(id: &SessionId, agent: &str) -> SessionReportAgentParams {
    SessionReportAgentParams {
        session_id: id.clone(),
        source: "pohunek:pi".to_owned(),
        agent: agent.to_owned(),
        activity: Some(AgentActivity::Working),
        seq: Some(ReportSequence::new(1)),
        pid: None,
        agent_session_id: None,
        agent_session_path: None,
    }
}

#[cfg(unix)]
#[tokio::test]
async fn callbacks_of_a_pinned_session_use_its_own_version_after_another_is_selected() {
    let dir = temp_dir("packaged-callback-select");
    let marker = dir.join("argv.txt");
    let (script, _gate) = assigned_agent_script(&dir, &marker);
    let packaged = PackagedPi::install_hooked("packaged-callback-select-plugins", &script);
    let registry = packaged.registry(&temp_store_path("packaged-callback-select"));
    let created = registry
        .create(packaged_params(&dir))
        .await
        .expect("create a package session");
    let v1_regions = detector_regions(&registry, &created.id).await;

    let second = crate::agent::host::fixture::install_pi_version(
        &packaged.plugins,
        &script,
        "2.0.0",
        crate::agent::host::fixture::PACKAGED_DETECT_MANIFEST_V2,
        false,
    );
    packaged
        .registry_handle()
        .select(&second)
        .expect("select v2");
    registry.reload_runtimes().expect("reload");
    let fresh = packaged
        .host
        .resolve_id(&protocol::RuntimeId::parse("pi").expect("id"))
        .expect("v2 serves fresh launches");
    let v2_regions = format!(
        "{:?}",
        crate::detect::DetectorConfig::for_definition(&fresh)
            .manifest
            .expect("manifest")
            .required_regions()
    );
    assert_ne!(v1_regions, v2_regions, "the versions differ");

    assert!(
        registry
            .report_agent(pi_report(&created.id, "pi"))
            .await
            .recorded
    );

    assert_eq!(
        detector_regions(&registry, &created.id).await,
        v1_regions,
        "a v1 session keeps v1 detection rules"
    );
    let _ = registry.stop(&created.id).await;
}

#[cfg(unix)]
#[tokio::test]
async fn callbacks_of_a_running_package_session_are_accepted_after_disable_and_reload() {
    let dir = temp_dir("packaged-callback-disable");
    let marker = dir.join("argv.txt");
    let (script, _gate) = assigned_agent_script(&dir, &marker);
    let packaged = PackagedPi::install_hooked("packaged-callback-disable-plugins", &script);
    let registry = packaged.registry(&temp_store_path("packaged-callback-disable"));
    let created = registry
        .create(packaged_params(&dir))
        .await
        .expect("create a package session");
    let regions = detector_regions(&registry, &created.id).await;
    packaged.disable_and_reload(&registry);

    assert!(
        registry
            .report_agent(pi_report(&created.id, "pi"))
            .await
            .recorded,
        "the running session's own runtime still reports"
    );
    assert_eq!(detector_regions(&registry, &created.id).await, regions);
    let info = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(info.active_agent.as_deref(), Some("pi"));
    let release = registry
        .release_agent(SessionReleaseAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:pi".to_owned(),
            agent: "pi".to_owned(),
            seq: Some(ReportSequence::new(2)),
        })
        .await;
    assert!(release.released, "and releases");
    let _ = registry.stop(&created.id).await;
}

/// Writes `<root>/<name>/interp`, a shim that prints `version` whatever its
/// arguments, and returns its directory.
#[cfg(unix)]
fn interpreter_dir(root: &std::path::Path, name: &str, version: &str) -> PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(&dir).expect("interpreter dir");
    write_executable(&dir.join("interp"), &format!("#!/bin/sh\necho {version}\n"));
    dir
}

/// A registry whose agents are `script`-backed Pi-shaped runtimes probed
/// under the `PATH` they launch with. The daemon's base environment carries
/// `base_path`; the profiles come from `agents_dir`.
#[cfg(unix)]
fn path_probed_registry(
    script: &std::path::Path,
    agents_dir: PathBuf,
    base_path: &std::path::Path,
) -> SessionRegistry {
    let crate::runtime::EnvironmentSource::Fixed(mut variables) =
        crate::test_support::thread_environment_source()
    else {
        panic!("a test fixture supplies an explicit environment");
    };
    variables.insert("PATH".into(), base_path.into());
    SessionRegistry::new_with_runtimes_and_environment(
        SessionRegistryConfig {
            shell_command: hermetic_shell(),
            stop_grace: Duration::from_millis(50),
            agents_dir: Some(agents_dir),
            ..SessionRegistryConfig::default()
        },
        crate::agent::host::fixture::pi_shaped_probed_host(
            script,
            crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
            crate::agent::host::fixture::PI_SHAPED_PROBE,
        ),
        crate::runtime::EnvironmentSource::Fixed(variables),
        pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
    )
}

#[cfg(unix)]
async fn create_in(
    registry: &SessionRegistry,
    agent: &str,
    cwd: &std::path::Path,
) -> Result<SessionInfo, protocol::ProtocolError> {
    registry
        .create(SessionNewParams {
            agent: agent.to_owned(),
            cwd: Some(cwd.to_path_buf()),
            ..params()
        })
        .await
}

#[cfg(unix)]
#[tokio::test]
async fn the_version_probe_runs_under_the_launch_path_of_every_launch_kind() {
    let root = temp_dir("probed-launch-path");
    let in_range = interpreter_dir(&root, "in-range", "1.0.2");
    let out_of_range = interpreter_dir(&root, "out-of-range", "2.0.0");
    let script = root.join("agent");
    write_executable(&script, "#!/usr/bin/env interp\n");
    let profile = |path: &std::path::Path| {
        format!(
            "base = \"pi\"\n{}program = \"{}\"\n[env]\nPATH = \"{}\"\n",
            crate::agent::host::fixture::pi_shaped_profile_pin(),
            script.display(),
            path.display()
        )
    };
    let agents_dir = temp_agents_dir_with("probed-launch-path", "pi-in", &profile(&in_range));
    fs::write(agents_dir.join("pi-out.toml"), profile(&out_of_range)).expect("profile");

    // The base PATH decides for an agent without a profile override, and the
    // profile PATH wins over it in both directions.
    for (base, bare_launches) in [(&in_range, true), (&out_of_range, false)] {
        let registry = path_probed_registry(&script, agents_dir.clone(), base);
        let capabilities = crate::capabilities::host_capabilities(
            "0.0.0",
            registry.profiles(),
            &registry.inspect_base_environment(),
        );
        for (agent, launches) in [("pi", bare_launches), ("pi-in", true), ("pi-out", false)] {
            let listed = capabilities
                .runtimes
                .iter()
                .find(|runtime| runtime.agent == agent)
                .expect("the agent is listed");
            assert_eq!(listed.supported, Some(launches), "inventory of {agent}");
            match create_in(&registry, agent, &root).await {
                Ok(created) => {
                    assert!(launches, "{agent} must be refused");
                    let _ = registry.stop(&created.id).await;
                }
                Err(error) => {
                    assert!(!launches, "{agent} must launch: {}", error.code);
                    assert_eq!(error.code, "agent_runtime_unsupported");
                }
            }
        }
    }

    // A relaunch resolves the profile again: the profile PATH picks the
    // interpreter although the base PATH holds an unsupported one.
    let registry = path_probed_registry(&script, agents_dir, &out_of_range);
    let created = create_in(&registry, "pi-in", &root)
        .await
        .expect("pi-in launches");
    registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("the session exits");
    let resumed = registry
        .resume(&created.id)
        .await
        .expect("resume probes under the profile PATH");
    let _ = registry.stop(&resumed.id).await;
}

/// One built-in agent of the migrated-binding tests: the v0.33.0 flat fields
/// its binding carried, and the argv its native launch must produce.
#[cfg(unix)]
struct MigratedAgentCase {
    agent: &'static str,
    frozen_args: &'static [&'static str],
    v0_33_0_fields: serde_json::Value,
    resume_argv: &'static [&'static str],
    fork_argv: Option<&'static [&'static str]>,
}

/// Records every argument vector it is launched with and stays alive; answers
/// the Hermes version probe, which the other agents never run.
#[cfg(unix)]
fn write_argv_recording_agent(path: &std::path::Path, marker: &std::path::Path) {
    write_supported_hermes_executable(
        path,
        &format!(
            "printf '%s\\n' \"$@\" >> '{}'\nsleep 30\n",
            marker.display()
        ),
    );
}

#[cfg(unix)]
#[tokio::test]
async fn migrated_legacy_bindings_resume_and_fork_with_the_native_argv() {
    let cases = [
        MigratedAgentCase {
            agent: "claude",
            frozen_args: &["--model", "sonnet"],
            v0_33_0_fields: serde_json::json!({
                "resume_mode": "flag", "ref_kind": "id", "resumable": true,
                "fork_mode": "claude_session", "fork_resume_mode": "flag",
                "fork_ref_kind": "id", "forkable": true,
            }),
            resume_argv: &["--resume", "native-legacy"],
            fork_argv: Some(&["--resume", "native-legacy", "--fork-session"]),
        },
        MigratedAgentCase {
            agent: "codex",
            frozen_args: &[],
            v0_33_0_fields: serde_json::json!({
                "resume_mode": "subcommand", "ref_kind": "id", "resumable": true,
            }),
            resume_argv: &["resume", "native-legacy"],
            fork_argv: None,
        },
        MigratedAgentCase {
            agent: "hermes",
            frozen_args: &["chat"],
            v0_33_0_fields: serde_json::json!({
                "resume_mode": "flag", "ref_kind": "id", "resumable": true,
            }),
            resume_argv: &["--resume", "native-legacy"],
            fork_argv: None,
        },
    ];
    // v0.33.0 froze the flat fields; v0.33.1 dropped them without a spec.
    for generation in ["v0.33.0", "v0.33.1"] {
        for case in &cases {
            assert_migrated_binding_argv(generation, case).await;
        }
    }
}

/// Migrates one generation's store line for `case`, then resumes and forks the
/// binding and checks the exact argv the agent is launched with.
#[cfg(unix)]
async fn assert_migrated_binding_argv(generation: &str, case: &MigratedAgentCase) {
    let tag = format!("migrated-{generation}-{}", case.agent);
    let dir = temp_dir(&tag);
    let marker = dir.join("argv.txt");
    let script = dir.join("agent");
    write_argv_recording_agent(&script, &marker);

    let mut line = serde_json::json!({
        "kind": "resume",
        "session_id": "s-4242",
        "agent": case.agent,
        "agent_base": case.agent,
        "cwd": dir,
        "cols": 80,
        "rows": 24,
        "native_session_id": "native-legacy",
        "program": script,
        "args": case.frozen_args,
        "input_rules": {"bracketed_paste": false, "submit_delay_ms": 150},
    });
    if generation == "v0.33.0" {
        line.as_object_mut()
            .expect("object")
            .extend(case.v0_33_0_fields.as_object().expect("fields").clone());
    }
    let store_path = temp_store_path(&tag);
    fs::write(&store_path, format!("{line}\n")).expect("write legacy store");
    fs::set_permissions(&store_path, fs::Permissions::from_mode(0o600)).expect("private store");
    crate::store::migrate_at_startup(&store_path).expect("migrate");
    let binding = crate::store::Store::new(store_path.clone())
        .load_resume()
        .expect("load migrated binding")
        .into_iter()
        .next()
        .expect("one binding");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: hermetic_shell(),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path),
        ..SessionRegistryConfig::default()
    });

    let resumed = registry
        .resume_binding(binding)
        .await
        .unwrap_or_else(|error| panic!("{tag}: resume failed: {error:?}"));

    let expected_resume = case
        .frozen_args
        .iter()
        .chain(case.resume_argv)
        .copied()
        .collect::<Vec<_>>();
    let launched = wait_for_line_count(&marker, expected_resume.len()).await;
    assert_eq!(
        launched.lines().collect::<Vec<_>>(),
        expected_resume,
        "{tag}: resume argv"
    );

    let fork = registry
        .fork(SessionForkParams {
            session_id: resumed.id.clone(),
            name: None,
            cwd_mode: ForkCwdMode::Same,
            cols: 80,
            rows: 24,
            accept_profile_change: false,
        })
        .await;
    match case.fork_argv {
        Some(fork_argv) => {
            let forked = fork.unwrap_or_else(|error| panic!("{tag}: fork failed: {error:?}"));
            let expected_fork = case
                .frozen_args
                .iter()
                .chain(fork_argv)
                .copied()
                .collect::<Vec<_>>();
            let total = expected_resume.len() + expected_fork.len();
            let launched = wait_for_line_count(&marker, total).await;
            assert_eq!(
                launched
                    .lines()
                    .skip(expected_resume.len())
                    .collect::<Vec<_>>(),
                expected_fork,
                "{tag}: fork argv"
            );
            let _ = registry.stop(&forked.id).await;
        }
        None => assert_eq!(
            fork.expect_err("the built-in spec has no fork").code,
            "agent_fork_unsupported",
            "{tag}"
        ),
    }
    let _ = registry.stop(&resumed.id).await;
}

/// Writes the script a hooked session runs: every real Codex and Claude
/// adapter once through the worker socket and, with the worker socket hidden,
/// once through the public daemon socket, then a completion marker.
///
/// Returns the script and the marker path.
#[cfg(unix)]
fn adapter_script(dir: &std::path::Path, public_only: bool) -> (PathBuf, PathBuf) {
    let assets = pohunek_test_support::manifest_dir().join("src/integration/assets");
    let a = assets.display();
    let done = dir.join("adapters.done");
    let script = dir.join("adapter-session");
    let hide = "env -u POHUNEK_WORKER_SOCKET_PATH";
    // With `public_only`, every adapter runs with the worker socket hidden.
    let first = if public_only { hide } else { "env" };
    write_executable(
        &script,
        &format!(
            "#!/bin/sh\n\
             printf '%s' '{{\"session_id\":\"native-codex\",\"transcript_path\":\"/t/codex.jsonl\"}}' | {first} sh '{a}/codex/pohunek-agent-state.sh' session\n\
             printf '%s' '{{\"session_id\":\"native-claude\",\"transcript_path\":\"/t/claude.jsonl\"}}' | {first} sh '{a}/claude/pohunek-agent-state.sh' session\n\
             printf '%s' '{{\"agent_id\":\"child-1\",\"agent_type\":\"explore\"}}' | {first} sh '{a}/codex/pohunek-agent-state.sh' subagent-start\n\
             printf '%s' '{{\"session_id\":\"native-public\",\"transcript_path\":\"/t/public.jsonl\"}}' | {hide} sh '{a}/codex/pohunek-agent-state.sh' session\n\
             printf '%s' '{{}}' | {first} sh '{a}/codex/pohunek-agent-notify.sh' stop\n\
             printf '%s' '{{}}' | {hide} sh '{a}/claude/pohunek-agent-notify.sh' stop\n\
             command -v python3 > '{done}.python'\n\
             : > '{done}'\n\
             exec sleep 30\n",
            done = done.display(),
        ),
    );
    (script, done)
}

/// A session registry over `host` (the built-in runtimes when `None`) with a
/// real control server on the daemon socket the session hooks fall back to.
#[cfg(unix)]
struct HookRig {
    registry: SessionRegistry,
    state: DaemonState,
    notifications: crate::notifications::NotificationService,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    server: tokio::task::JoinHandle<()>,
}

#[cfg(unix)]
impl HookRig {
    async fn start(
        dir: &std::path::Path,
        host: Option<crate::agent::host::RuntimeHost>,
        shell_command: ShellCommand,
    ) -> Self {
        // The in-process worker launcher gives sessions `test-daemon.sock` below
        // the worker runtime root as their daemon socket.
        let runtime_root = crate::test_support::thread_scoped_dir("pw-hook-");
        let socket = runtime_root.join("test-daemon.sock");
        let config = SessionRegistryConfig {
            shell_command,
            stop_grace: Duration::from_millis(50),
            worker_runtime_root: Some(runtime_root),
            ..SessionRegistryConfig::default()
        };
        let registry = match host {
            Some(host) => SessionRegistry::new_with_runtimes(config, host),
            None => SessionRegistry::new(config),
        };
        let notifications =
            crate::notifications::NotificationService::open(&dir.join("notifications"))
                .expect("notification service opens");
        let state = DaemonState::new(
            HealthInfo::new("test"),
            registry.clone(),
            Arc::new(crate::governance::HostGovernanceService::open_test()),
            crate::test_support::overlay_registry(),
        )
        .with_notifications(notifications.clone());
        let server = crate::api::ControlServer::bind_with_state(&socket, state.clone())
            .await
            .expect("control server binds");
        let (shutdown, stop) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            server
                .serve(async move {
                    let _ = stop.await;
                })
                .await;
        });
        Self {
            registry,
            state,
            notifications,
            shutdown: Some(shutdown),
            server,
        }
    }

    /// Dispatches one public request and returns its response.
    async fn request(&self, method: &str, params: serde_json::Value) -> Response {
        let request = Request::new("hook-rig", method, params).expect("valid request");
        let line = serde_json::to_string(&request).expect("request serializes");
        let crate::api::Dispatch::Reply(reply) = dispatch_line(&line, &self.state, None).await
        else {
            panic!("{method} returns a one-shot reply");
        };
        serde_json::from_str(&reply).expect("response deserializes")
    }

    /// Stops the server and the session.
    async fn finish(mut self, id: &SessionId) {
        let _ = self.registry.stop(id).await;
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.server.await;
    }
}

/// Waits for the adapter script of a session to finish.
#[cfg(unix)]
async fn wait_for_adapters(done: &std::path::Path) {
    wait_until("the adapter script to finish", || async {
        done.exists().then_some(())
    })
    .await;
    // Each adapter answers only after the worker or daemon handled its report,
    // so state is settled once the script is done. A rejection test proves
    // nothing if the adapters could not run, so the interpreter must exist.
    let python = fs::read_to_string(format!("{}.python", done.display())).unwrap_or_default();
    assert!(
        !python.trim().is_empty(),
        "the adapters need python3 on PATH"
    );
}

/// Asserts that the worker journaled the adapters' identity and subagent and
/// that the daemon imports that snapshot.
///
/// The daemon's own `active_agent` projection is not observed: procwatch
/// releases a claim that no foreground process backs within a tick, so it is
/// transient. The worker journal is written before the adapter is answered,
/// and applying the snapshot returns once the daemon has committed it.
#[cfg(unix)]
async fn assert_worker_state_imported(registry: &SessionRegistry, id: &SessionId) {
    let (worker, _identity) = live_worker_and_identity(registry, id).await;
    let snapshot = worker.inspect().await.expect("inspect worker");
    assert!(snapshot.active_identity.is_some(), "identity journaled");
    assert_eq!(snapshot.subagents.len(), 1, "subagent journaled");
    let outcome = wait_until("the daemon to settle the worker snapshot", || async {
        match Box::pin(registry.apply_worker_metadata_snapshot(id, &snapshot)).await {
            WorkerMetadataApplyOutcome::Retryable(_) => None,
            settled => Some(settled),
        }
    })
    .await;
    assert_eq!(
        outcome,
        WorkerMetadataApplyOutcome::Applied,
        "the daemon imports the admitted claims"
    );
}

/// The public notification request a hook of `provider` sends for `session`.
#[cfg(unix)]
fn hook_notification(provider: &str, session: &SessionId) -> serde_json::Value {
    serde_json::json!({
        "source": {
            "provider": provider,
            "provider_event": "Stop",
            "host_local_source_id": format!("hook:{provider}:Stop:e1"),
        },
        "kind": "error", "severity": "error", "title": "Agent error",
        "body": "The agent reported an error.", "metadata": {}, "session_id": session,
        "agent_kind": provider, "source_id": format!("hook:{provider}:Stop:e1"),
    })
}

#[tokio::test]
#[cfg(unix)]
async fn a_runtime_without_a_hook_schema_rejects_every_adapter_report_on_both_sockets() {
    let dir = temp_dir("hook-schema-negative");
    let (script, done) = adapter_script(&dir, false);
    let host = crate::agent::host::fixture::pi_shaped_host(
        &script,
        crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
    );
    let rig = HookRig::start(&dir, Some(host), hermetic_shell()).await;
    let created = rig
        .registry
        .create(SessionNewParams {
            agent: "pi".to_owned(),
            cwd: Some(dir.clone()),
            ..params()
        })
        .await
        .expect("create session");
    let launch_reference = created.native_session_id.clone();
    wait_for_adapters(&done).await;

    let info = rig.registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(info.active_agent, None, "no adapter report set an identity");
    assert_eq!(info.native_session_id, launch_reference);
    assert!(
        rig.registry
            .inner
            .sessions
            .lock()
            .await
            .get(&created.id)
            .is_some_and(|entry| entry.last_agent_report.is_none()),
        "the public fallback recorded nothing"
    );
    assert!(info.subagents.is_empty());
    let (worker, _identity) = live_worker_and_identity(&rig.registry, &created.id).await;
    let snapshot = worker.inspect().await.expect("inspect worker");
    assert!(snapshot.active_identity.is_none());
    assert!(snapshot.launch_identity.is_none());
    assert!(snapshot.subagents.is_empty());

    let report_agent = serde_json::json!({
        "session_id": created.id, "source": "pohunek:codex", "agent": "codex", "seq": "1",
    });
    let response = rig
        .request(method::SESSION_REPORT_AGENT, report_agent)
        .await;
    assert_eq!(response.into_result().expect("reply")["recorded"], false);
    let release = serde_json::json!({
        "session_id": created.id, "source": "pohunek:codex", "agent": "codex", "seq": "2",
    });
    let response = rig.request(method::SESSION_RELEASE_AGENT, release).await;
    assert_eq!(response.into_result().expect("reply")["released"], false);
    let notification = rig
        .request(
            method::NOTIFICATION_CREATE,
            hook_notification("codex", &created.id),
        )
        .await;
    assert_eq!(
        notification.into_result().expect_err("refused").code,
        "hook_not_admitted"
    );
    let listed = rig
        .notifications
        .list(protocol::NotificationListParams::default())
        .expect("list notifications");
    assert!(
        listed.notifications.is_empty(),
        "no notification was created"
    );

    rig.finish(&created.id).await;
}

#[tokio::test]
#[cfg(unix)]
async fn a_runtime_with_a_hook_schema_accepts_the_adapter_reports_on_both_sockets() {
    let dir = temp_dir("hook-schema-positive");
    let (script, done) = adapter_script(&dir, false);
    let host = crate::agent::host::fixture::pi_shaped_hooked_host(
        &script,
        crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
    );
    let rig = HookRig::start(&dir, Some(host), hermetic_shell()).await;
    let created = rig
        .registry
        .create(SessionNewParams {
            agent: "pi".to_owned(),
            cwd: Some(dir.clone()),
            ..params()
        })
        .await
        .expect("create session");
    wait_for_adapters(&done).await;

    Box::pin(assert_worker_state_imported(&rig.registry, &created.id)).await;
    let notification = rig
        .request(
            method::NOTIFICATION_CREATE,
            hook_notification("codex", &created.id),
        )
        .await;
    notification
        .into_result()
        .expect("the hooked notification is created");

    rig.finish(&created.id).await;
}

#[tokio::test]
#[cfg(unix)]
async fn the_public_socket_alone_accepts_adapter_reports_of_a_runtime_with_a_hook_schema() {
    let dir = temp_dir("hook-schema-public");
    let (script, done) = adapter_script(&dir, true);
    let host = crate::agent::host::fixture::pi_shaped_hooked_host(
        &script,
        crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
    );
    let rig = HookRig::start(&dir, Some(host), hermetic_shell()).await;
    let created = rig
        .registry
        .create(SessionNewParams {
            agent: "pi".to_owned(),
            cwd: Some(dir.clone()),
            ..params()
        })
        .await
        .expect("create session");
    wait_for_adapters(&done).await;

    let source = rig
        .registry
        .inner
        .sessions
        .lock()
        .await
        .get(&created.id)
        .and_then(|entry| entry.last_agent_report.as_ref().map(|r| r.source.clone()));
    assert!(
        source.is_some_and(|source| source.starts_with("pohunek:")),
        "the public socket recorded an adapter report"
    );
    let (worker, _identity) = live_worker_and_identity(&rig.registry, &created.id).await;
    let snapshot = worker.inspect().await.expect("inspect worker");
    assert!(
        snapshot.active_identity.is_none(),
        "the worker was not used"
    );

    rig.finish(&created.id).await;
}

#[tokio::test]
#[cfg(unix)]
async fn a_shell_session_accepts_the_adapter_reports_of_the_agents_it_hosts() {
    let dir = temp_dir("hook-schema-shell");
    let (script, done) = adapter_script(&dir, false);
    let command = ShellCommand::new("/bin/sh", [script.to_str().expect("utf-8 path")]);
    let rig = HookRig::start(&dir, None, command).await;
    let created = rig
        .registry
        .create(SessionNewParams {
            cwd: Some(dir.clone()),
            ..params()
        })
        .await
        .expect("create session");
    wait_for_adapters(&done).await;

    Box::pin(assert_worker_state_imported(&rig.registry, &created.id)).await;
    for provider in ["codex", "claude"] {
        let response = rig
            .request(
                method::NOTIFICATION_CREATE,
                hook_notification(provider, &created.id),
            )
            .await;
        let result = response.into_result();
        assert!(result.is_ok(), "{provider}: {result:?}");
    }

    rig.finish(&created.id).await;
}

#[tokio::test]
#[cfg(unix)]
async fn the_identity_schema_admits_hermes_reports_and_refuses_unlisted_sessions() {
    let dir = temp_dir("hook-schema-hermes");
    let rig = HookRig::start(&dir, None, hermetic_shell()).await;
    let created = rig
        .registry
        .create(SessionNewParams {
            cwd: Some(dir.clone()),
            ..params()
        })
        .await
        .expect("create session");

    let hermes = serde_json::json!({
        "session_id": created.id, "source": "pohunek:hermes", "agent": "hermes", "seq": "1",
    });
    let response = rig.request(method::SESSION_REPORT_AGENT, hermes).await;
    assert_eq!(response.into_result().expect("reply")["recorded"], true);
    let notification = rig
        .request(
            method::NOTIFICATION_CREATE,
            hook_notification("hermes", &created.id),
        )
        .await;
    notification
        .into_result()
        .expect("the hooked notification is created");

    rig.finish(&created.id).await;
}
