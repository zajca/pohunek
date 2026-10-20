//! Forwarded active identities against a real `pohunek-sessiond`.
//!
//! The daemon's runtime client forwards a report and a release into a real
//! worker whose PTY tree holds the subject, and a second connection reads the
//! journaled result back, as a restarted daemon would. A previous-version
//! connection reads the same journal without the forwarding fields.
//!
//! Build the worker first: `cargo build -p pohunek-session-worker --bin
//! pohunek-sessiond`.

use std::collections::BTreeMap;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use pohunek_daemon::runtime::{Worker, WorkerError};
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::wait_until;
use pohunek_test_support::worker_binary;
use pohunek_worker_protocol::{
    ActiveIdentityOutcome, BaseEnv, ClaimOrigin, ControlCode, DaemonId, Dimensions,
    ForwardedActiveRelease, ForwardedActiveReport, IdentityRejectReason, Initialize,
    InitializeLimits, InputPolicy, LaunchIdentity, ProcessIdentity, ReportedSession,
    ReportedSessionKind, SecretEnv, SessionId, StopPolicy, TransactionId, Version, CURRENT_VERSION,
    PREVIOUS_VERSION,
};

const DEADLINE: Duration = Duration::from_secs(15);
const DAEMON_ID: &str = "daemon-forwarded-identity";
const GENERATION: &str = "abcd2345";
const HOOK_SCHEMA: &str = "identity-subagent-v1";
/// Seconds a forwarded claim lives, well inside the shared 60 s ceiling.
const CLAIM_LEASE_SECS: i64 = 30;

static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    root: PathBuf,
    session_id: String,
    notify: UnixDatagram,
    worker: tokio::process::Child,
    _env: TestEnv,
}

impl Fixture {
    fn start() -> Self {
        let binary = worker_binary();
        assert!(
            binary.is_file(),
            "{} is missing; build pohunek-sessiond first",
            binary.display()
        );
        let sequence = FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let env = TestEnv::new().expect("create the fixture environment");
        let root = env.root().to_path_buf();
        for directory in [
            "runtime/pohunek/workers",
            "state/pohunek/workers",
            "data/pohunek",
            "config/pohunek",
            "cache/pohunek",
            "home",
        ] {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(root.join(directory))
                .expect("create fixture directory");
        }
        let notify_path = root.join("notify.sock");
        let notify = UnixDatagram::bind(&notify_path).expect("bind notify socket");
        notify
            .set_read_timeout(Some(DEADLINE))
            .expect("notify read timeout");
        let session_id = format!("s-{}{sequence}", std::process::id());
        let worker = tokio::process::Command::new(binary)
            .args(["--session-id", &session_id])
            .args(["--worker-generation", GENERATION])
            .arg("--daemon-socket-path")
            .arg(root.join("runtime/pohunek/daemon.sock"))
            .env_clear()
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("HOME", root.join("home"))
            .env("NOTIFY_SOCKET", &notify_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn pohunek-sessiond");
        let mut ready = [0_u8; 256];
        let length = notify.recv(&mut ready).expect("worker readiness");
        assert!(ready[..length].starts_with(b"READY=1"));
        Self {
            root,
            session_id,
            notify,
            worker,
            _env: env,
        }
    }

    fn socket_path(&self) -> PathBuf {
        self.root
            .join("runtime/pohunek/workers")
            .join(&self.session_id)
            .join(pohunek_paths::WORKER_SOCKET_NAME)
    }

    /// Connects a controller whose protocol range ends at `maximum`.
    async fn connect(&mut self, maximum: Version) -> Worker {
        let socket = self.socket_path();
        let child = std::cell::RefCell::new(&mut self.worker);
        let session_id = &self.session_id;
        wait_until("the worker to accept a controller", || async {
            match Worker::connect_with_range(
                &socket,
                session_id,
                DAEMON_ID,
                PREVIOUS_VERSION,
                maximum,
            )
            .await
            {
                Ok(worker) => Some(worker),
                Err(error) => {
                    assert!(
                        child
                            .borrow_mut()
                            .try_wait()
                            .expect("inspect worker")
                            .is_none(),
                        "worker exited before accepting a controller: {error}"
                    );
                    None
                }
            }
        })
        .await
    }

    /// Runs a child that outlives the test inside the worker's PTY.
    fn initialize(
        &self,
        worker: &Worker,
        worker_id: pohunek_worker_protocol::WorkerId,
    ) -> Initialize {
        let _ = worker;
        Initialize {
            session_id: SessionId::new(&self.session_id).expect("session id"),
            daemon_instance_id: Some(DaemonId::new(DAEMON_ID).expect("daemon id")),
            transaction_id: TransactionId::new("create-forwarded").expect("transaction id"),
            expected_worker_id: worker_id,
            launch: LaunchIdentity {
                agent: "claude".to_owned(),
                agent_base: "claude".to_owned(),
                reference_kind: None,
            },
            executable: PathBuf::from("/bin/sh"),
            arguments: vec!["-c".to_owned(), "exec sleep 3600".to_owned()],
            cwd: self.root.clone(),
            dimensions: Dimensions::new(80, 24).expect("dimensions"),
            environment: SecretEnv::new(BTreeMap::new()).expect("environment"),
            base_environment: BaseEnv::new(BTreeMap::from([
                ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
                ("SHELL".to_owned(), "/bin/sh".to_owned()),
            ]))
            .expect("base environment"),
            limits: InitializeLimits::new(1_048_576, 1_048_576, 128, 60_000).expect("limits"),
            stop_policy: StopPolicy::new(500).expect("stop policy"),
            input_policy: InputPolicy::new(3000).expect("input policy"),
            hook_protocol_version: Version::new(1).expect("hook version"),
            public_protocol_version: 8,
            hook_schema: Some(HOOK_SCHEMA.to_owned()),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.worker.start_kill();
        let _ = &self.notify;
    }
}

fn claim_expiry() -> String {
    (time::OffsetDateTime::now_utc() + time::Duration::seconds(CLAIM_LEASE_SECS))
        .format(&time::format_description::well_known::Rfc3339)
        .expect("format expiry")
}

/// Starts the worker, initializes the sleeping child and returns the
/// controller together with the child as the subject of reports.
async fn running_fixture() -> (Fixture, Worker, ProcessIdentity) {
    let mut fixture = Fixture::start();
    let worker = fixture.connect(CURRENT_VERSION).await;
    assert_eq!(worker.selected_version().await, CURRENT_VERSION);
    let initialize = fixture.initialize(&worker, worker.worker_id().await);
    worker.initialize(initialize).await.expect("initialize");
    let child = worker
        .inspect()
        .await
        .expect("inspect")
        .child_process
        .expect("the child runs");
    (fixture, worker, child)
}

fn report(subject: ProcessIdentity, sequence: u64) -> ForwardedActiveReport {
    ForwardedActiveReport {
        provider: "claude".to_owned(),
        agent: Some("tuned".to_owned()),
        process: subject,
        sequence,
        expires_at: claim_expiry(),
        reported_session: Some(ReportedSession {
            kind: ReportedSessionKind::Id,
            value: "annotation-1".to_owned(),
            transcript_path: None,
        }),
    }
}

async fn stop(worker: &Worker) {
    worker
        .stop(TransactionId::new("forwarded-stop").expect("transaction id"))
        .await
        .expect("stop the child");
}

#[tokio::test]
async fn a_forwarded_report_is_durable_across_a_controller_reconnect() {
    let (mut fixture, worker, child) = running_fixture().await;

    let outcome = worker
        .record_active_identity(report(child, 100))
        .await
        .expect("forward the report");
    assert_eq!(outcome, ActiveIdentityOutcome::Accepted { revision: 1 });

    worker.release_controller().await.expect("release lease");
    drop(worker);
    let reconnected = fixture.connect(CURRENT_VERSION).await;
    let snapshot = reconnected
        .inspect()
        .await
        .expect("inspect after reconnect");
    let claim = snapshot.active_identity.expect("the journaled claim");
    assert_eq!(claim.provider, "claude");
    assert_eq!(claim.process, child);
    assert_eq!(claim.sequence, 100);
    assert_eq!(claim.origin, Some(ClaimOrigin::Forwarded));
    assert_eq!(claim.agent.as_deref(), Some("tuned"));
    assert_eq!(
        claim.reported_session.map(|session| session.value),
        Some("annotation-1".to_owned())
    );
    assert_eq!((claim.reference_kind, claim.native_reference), (None, None));
    assert_eq!(snapshot.active_identity_revision, Some(1));
    assert!(snapshot.launch_identity.is_none());
    assert!(snapshot.native_reference.is_none());
    stop(&reconnected).await;
}

#[tokio::test]
async fn a_previous_version_connection_reads_a_forwarded_claim_as_provider_only() {
    let (mut fixture, worker, child) = running_fixture().await;
    worker
        .record_active_identity(report(child, 110))
        .await
        .expect("forward the report");
    worker.release_controller().await.expect("release lease");
    drop(worker);

    let previous = fixture.connect(PREVIOUS_VERSION).await;
    assert_eq!(previous.selected_version().await, PREVIOUS_VERSION);
    let snapshot = previous
        .inspect()
        .await
        .expect("inspect at the previous version");
    let claim = snapshot.active_identity.expect("the claim is still there");
    assert_eq!(claim.provider, "claude");
    assert_eq!(
        (claim.origin, claim.agent, claim.reported_session),
        (None, None, None)
    );
    assert_eq!((claim.reference_kind, claim.native_reference), (None, None));
    assert_eq!(snapshot.active_identity_revision, None);

    let refused = previous
        .record_active_identity(report(child, 111))
        .await
        .expect_err("a previous-version connection cannot forward");
    assert!(
        matches!(
            refused,
            WorkerError::Rejected {
                code: ControlCode::WorkerFeatureUnavailable,
                ..
            }
        ),
        "{refused:?}"
    );
    stop(&previous).await;
}

#[tokio::test]
async fn a_matching_release_survives_a_reconnect_and_a_stale_one_is_refused() {
    let (mut fixture, worker, child) = running_fixture().await;
    worker
        .record_active_identity(report(child, 120))
        .await
        .expect("forward the report");

    let stale = worker
        .release_active_identity(ForwardedActiveRelease {
            provider: "claude".to_owned(),
            process: child,
            sequence: 120,
        })
        .await
        .expect("forward the stale release");
    assert_eq!(
        stale,
        ActiveIdentityOutcome::Rejected {
            reason: IdentityRejectReason::SequenceStale
        }
    );

    let released = worker
        .release_active_identity(ForwardedActiveRelease {
            provider: "claude".to_owned(),
            process: child,
            sequence: 121,
        })
        .await
        .expect("forward the release");
    assert_eq!(released, ActiveIdentityOutcome::Accepted { revision: 2 });

    worker.release_controller().await.expect("release lease");
    drop(worker);
    let reconnected = fixture.connect(CURRENT_VERSION).await;
    let snapshot = reconnected
        .inspect()
        .await
        .expect("inspect after reconnect");
    assert!(snapshot.active_identity.is_none());
    let tombstone = snapshot
        .active_identity_release
        .expect("the journaled release");
    assert_eq!(
        (tombstone.provider.as_str(), tombstone.sequence),
        ("claude", 121)
    );
    assert_eq!(snapshot.active_identity_revision, Some(2));
    stop(&reconnected).await;
}

#[tokio::test]
async fn a_subject_outside_the_session_tree_or_with_a_reused_start_identity_changes_nothing() {
    let (mut fixture, worker, child) = running_fixture().await;
    let outside = ProcessIdentity {
        pid: std::process::id(),
        start_identity: 1,
    };
    let reused = ProcessIdentity {
        start_identity: child.start_identity + 1,
        ..child
    };

    for subject in [outside, reused] {
        let outcome = worker
            .record_active_identity(report(subject, 130))
            .await
            .expect("forward the hostile report");
        assert_eq!(
            outcome,
            ActiveIdentityOutcome::Rejected {
                reason: IdentityRejectReason::ReportedProcessInvalid
            },
            "{subject:?}"
        );
    }

    worker.release_controller().await.expect("release lease");
    drop(worker);
    let reconnected = fixture.connect(CURRENT_VERSION).await;
    let snapshot = reconnected
        .inspect()
        .await
        .expect("inspect after reconnect");
    assert!(snapshot.active_identity.is_none());
    assert!(snapshot.active_identity_release.is_none());
    assert_eq!(snapshot.active_identity_revision, Some(0));
    stop(&reconnected).await;
}

/// A controller that negotiated the previous protocol version acknowledges a
/// terminal runtime the same way: the worker journals the acknowledgement and
/// exits.
#[tokio::test]
async fn previous_protocol_worker_accepts_terminal_acknowledgement() {
    let (mut fixture, worker, _child) = running_fixture().await;
    worker.release_controller().await.expect("release lease");
    drop(worker);

    let previous = fixture.connect(PREVIOUS_VERSION).await;
    assert_eq!(previous.selected_version().await, PREVIOUS_VERSION);
    assert!(
        previous.acknowledge_terminal().await.is_err(),
        "a running worker refuses the acknowledgement"
    );
    stop(&previous).await;
    previous
        .acknowledge_terminal()
        .await
        .expect("a terminal worker accepts the acknowledgement");

    let worker_process = std::cell::RefCell::new(&mut fixture.worker);
    wait_until("the acknowledged worker to exit", || async {
        worker_process
            .borrow_mut()
            .try_wait()
            .expect("inspect the worker process")
            .map(|_status| ())
    })
    .await;
}
