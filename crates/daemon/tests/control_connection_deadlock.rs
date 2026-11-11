//! The control connection between the daemon and a session worker never
//! deadlocks, and a phase that cannot finish ends the connection.
//!
//! A real in-process `pohunek_session_worker::Server` runs a real PTY child
//! and the daemon's runtime client talks to it over sockets whose send and
//! receive buffers are set to a few KiB, so Linux buffers like macOS does. A
//! scripted peer on a raw listener stands in where the worker must misbehave
//! (never read, never answer, answer unasked), and a raw client stands in for
//! a daemon that reads the connection only inside an exchange.

// Rust guideline compliant 2026-11-06

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pohunek_daemon::runtime::{ExchangeDeadlines, ExchangePhase, Worker, WorkerError};
use pohunek_session_worker::{Server, ServerArgs, WorkerConfig};
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::{guard, wait_until};
use pohunek_worker_protocol::{
    BaseEnv, Capability, ControlCode, ControlError, ControlMessage, ControlReader, ControlRequest,
    ControlResponse, ControlWriter, DaemonId, Dimensions, EventKind, Initialize, InitializeLimits,
    InputFragment, InputPolicy, LaunchIdentity, LeaseChallenge, LeaseId, ProcessIdentity,
    RequestId, RequestKind, ResponseKind, RuntimePhase, SecretBytes, SecretEnv, SessionId,
    StopPolicy, TransactionId, Version, WorkerId, WorkerInstanceId, CURRENT_VERSION,
    SUPPORTED_RANGE,
};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};

const DAEMON_ID: &str = "daemon-control-deadlock";
const SESSION_ID: &str = "s-9001";
const WORKER_ID: &str = "worker-control-deadlock";
const GENERATION: &str = "abcd2345";

/// Send and receive buffer both ends of the control socket are set to; the
/// kernel rounds it up (Linux doubles it, macOS keeps a minimum).
const SOCKET_BUFFER_BYTES: usize = 8 * 1024;

/// Lines the child prints before it reads input: far more events than the
/// socket buffers hold, each in a read of its own.
const CHILD_LINES: u32 = 20_000;

/// File the child creates once it printed every line.
const PRINTED_MARKER: &str = "printed";

/// Argument size of a request that cannot fit any socket buffer, below the
/// control line limit.
const OVERSIZED_ARGUMENT_BYTES: usize = 900_000;

/// Deadlines the scripted-peer scenarios set; they run on the paused clock.
const SCRIPTED_DEADLINES: ExchangeDeadlines = ExchangeDeadlines {
    // timing-allowed: #1983 the exchange deadline is the policy under test; the scenarios run on the paused clock
    send: Duration::from_secs(5),
    answer: Duration::from_secs(7),
};

/// A byte that encodes to three digits and a comma in a JSON number array.
const WIDE_BYTE: u8 = b'x';

fn session_id() -> SessionId {
    SessionId::new(SESSION_ID).expect("session id")
}

fn worker_id() -> WorkerId {
    WorkerId::new(WORKER_ID).expect("worker id")
}

fn initialize(root: &Path, arguments: Vec<String>) -> Initialize {
    Initialize {
        session_id: session_id(),
        daemon_instance_id: Some(DaemonId::new(DAEMON_ID).expect("daemon id")),
        transaction_id: TransactionId::new("create-control-deadlock").expect("transaction id"),
        expected_worker_id: worker_id(),
        launch: LaunchIdentity {
            agent: "shell".to_owned(),
            agent_base: "shell".to_owned(),
            reference_kind: None,
        },
        executable: PathBuf::from("/bin/sh"),
        arguments,
        cwd: root.to_path_buf(),
        dimensions: Dimensions::new(80, 24).expect("dimensions"),
        environment: SecretEnv::new(BTreeMap::new()).expect("environment"),
        base_environment: BaseEnv::new(BTreeMap::from([
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ("SHELL".to_owned(), "/bin/sh".to_owned()),
        ]))
        .expect("base environment"),
        limits: InitializeLimits::new(1_048_576, 1_048_576, 128, 60_000).expect("limits"),
        stop_policy: StopPolicy::new(500).expect("stop policy"),
        input_policy: InputPolicy::new(30_000).expect("input policy"),
        hook_protocol_version: Version::new(1).expect("hook version"),
        public_protocol_version: 8,
        hook_schema: None,
        answers_program_status_probe: false,
        probe_foreground_programs: Vec::new(),
        placement_check_interval_ms: None,
        handshake_file: None,
    }
}

fn input_of(bytes: usize) -> Vec<InputFragment> {
    vec![InputFragment {
        bytes: SecretBytes::new(vec![WIDE_BYTE; bytes]),
        delay_after_ms: 0,
    }]
}

/// A real worker server bound under a scenario directory.
struct Served {
    root: PathBuf,
    socket: PathBuf,
    server: tokio::task::JoinHandle<()>,
    _env: TestEnv,
}

impl Served {
    async fn start(buffer: Option<usize>) -> Self {
        let env = TestEnv::new().expect("create the fixture environment");
        let root = env.root().to_path_buf();
        let socket = root.join("socket/worker.sock");
        let mut config = WorkerConfig::new();
        config.control_socket_buffer_bytes = buffer;
        let server = Server::bind(ServerArgs {
            session_id: SESSION_ID.to_owned(),
            worker_id: WORKER_ID.to_owned(),
            generation: GENERATION.to_owned(),
            socket_path: socket.clone(),
            journal_path: root.join("journal/worker.json"),
            daemon_socket_path: root.join("daemon.sock"),
            hook_trace_path: root.join(pohunek_logging::config::HOOK_TRACE_FILE_NAME),
            config,
        })
        .await
        .expect("bind the worker");
        let server = tokio::spawn(async move {
            let _ = server.serve().await;
        });
        Self {
            root,
            socket,
            server,
            _env: env,
        }
    }

    /// The shell child: prints `CHILD_LINES` lines, marks it, then reads input.
    fn child(&self) -> Initialize {
        let script = format!(
            "stty raw -echo; i=0; while [ $i -lt {CHILD_LINES} ]; do echo $i; i=$((i+1)); done; : > {PRINTED_MARKER}; exec cat >/dev/null"
        );
        initialize(&self.root, vec!["-c".to_owned(), script])
    }

    async fn printed(&self) {
        let marker = self.root.join(PRINTED_MARKER);
        wait_until("the child to print every line", || async {
            marker.exists().then_some(())
        })
        .await;
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// A request larger than both buffers sent after an event backlog is answered,
/// and the worker keeps answering afterwards.
#[tokio::test]
async fn an_input_larger_than_the_buffers_after_an_event_backlog_is_answered() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let served = Served::start(Some(SOCKET_BUFFER_BYTES)).await;
    let worker = Worker::connect_with_socket_buffers(
        &served.socket,
        SESSION_ID,
        DAEMON_ID,
        SOCKET_BUFFER_BYTES,
    )
    .await
    .expect("connect with small buffers");
    let (sent, received) = worker
        .socket_buffer_bytes()
        .await
        .expect("read the granted buffers back");
    worker
        .initialize(served.child())
        .await
        .expect("initialize the runtime");

    // No exchange runs while the child prints, so the events pile up.
    served.printed().await;

    // Four wire bytes per input byte: the request exceeds both buffers twice.
    let input = sent.max(received);
    let written = guard(
        "an input larger than the socket buffers",
        worker.write(input_of(input)),
    )
    .await
    .expect("the input is answered");
    assert_eq!(written, u64::try_from(input).expect("input fits u64"));

    guard("the terminal snapshot", worker.terminal_snapshot())
        .await
        .expect("the screen is answered");
    guard("the inspection", worker.inspect())
        .await
        .expect("the inspection is answered");
}

/// A scripted worker behind a raw listener.
struct Scripted {
    reader: ControlReader<OwnedReadHalf>,
    writer: ControlWriter<OwnedWriteHalf>,
}

impl Scripted {
    /// Accepts one controller and answers its handshake.
    async fn accept(listener: &UnixListener) -> Self {
        let (stream, _) = listener.accept().await.expect("accept the controller");
        let (read_half, write_half) = stream.into_split();
        let mut peer = Self {
            reader: ControlReader::new(read_half),
            writer: ControlWriter::new(write_half),
        };
        let request = peer.request().await;
        assert!(matches!(request.kind, RequestKind::Negotiate { .. }));
        peer.respond(
            request.request_id,
            ResponseKind::Negotiated {
                selected_version: CURRENT_VERSION,
                supported_range: SUPPORTED_RANGE,
                session_id: session_id(),
                worker_id: worker_id(),
                worker_instance_id: Some(
                    WorkerInstanceId::new("runtime-control-deadlock").expect("runtime id"),
                ),
                worker_process: ProcessIdentity {
                    pid: std::process::id(),
                    start_identity: 1,
                },
                phase: RuntimePhase::Running,
                capabilities: vec![Capability::DeduplicatedInput],
                challenge: LeaseChallenge::new("challenge-control-deadlock").expect("challenge"),
            },
        )
        .await;
        let request = peer.request().await;
        assert!(matches!(
            request.kind,
            RequestKind::AcquireController { .. }
        ));
        peer.respond(
            request.request_id,
            ResponseKind::ControllerAcquired {
                lease_id: LeaseId::new("lease-control-deadlock").expect("lease id"),
                capabilities: vec![Capability::DeduplicatedInput],
            },
        )
        .await;
        peer
    }

    async fn request(&mut self) -> ControlRequest {
        match self
            .reader
            .read::<ControlMessage>()
            .await
            .expect("read a control message")
            .expect("the controller stays connected")
        {
            ControlMessage::Request(request) => request,
            other => panic!("expected a request, read {other:?}"),
        }
    }

    async fn respond(&mut self, request_id: RequestId, kind: ResponseKind) {
        self.writer
            .write(&ControlMessage::Response(ControlResponse {
                request_id,
                kind,
            }))
            .await
            .expect("write a response");
        self.writer.flush().await.expect("flush a response");
    }

    /// Reads until the controller's end of the connection is gone.
    async fn until_closed(&mut self) {
        loop {
            match self.reader.read::<ControlMessage>().await {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return,
            }
        }
    }
}

fn rejection() -> ResponseKind {
    ResponseKind::Error {
        error: ControlError {
            code: ControlCode::RuntimeFault,
            message: "scripted".to_owned(),
            retryable: false,
        },
    }
}

/// A listener under a scenario directory and a controller connected to it.
async fn scripted_pair() -> (TestEnv, UnixListener, Worker, Scripted) {
    let env = TestEnv::new().expect("create the fixture environment");
    let socket = env.root().join("scripted.sock");
    let listener = UnixListener::bind(&socket).expect("bind the scripted worker");
    let peer = tokio::spawn({
        let accept = async move {
            let peer = Scripted::accept(&listener).await;
            (listener, peer)
        };
        accept
    });
    let worker = Worker::connect(&socket, SESSION_ID, DAEMON_ID)
        .await
        .expect("connect to the scripted worker");
    worker.set_exchange_deadlines(SCRIPTED_DEADLINES);
    let (listener, peer) = peer.await.expect("the scripted handshake finishes");
    (env, listener, worker, peer)
}

/// A worker that stops reading ends the send at the send deadline: the
/// connection is closed, the worker sees the end of it, and later exchanges
/// fail without hanging.
#[tokio::test(start_paused = true)]
async fn a_worker_that_stops_reading_ends_the_send_and_the_connection() {
    let (env, _listener, worker, mut peer) = scripted_pair().await;
    let oversized = initialize(env.root(), vec!["x".repeat(OVERSIZED_ARGUMENT_BYTES)]);

    let error = worker
        .initialize(oversized)
        .await
        .expect_err("the request cannot be written to a worker that does not read");
    assert!(
        matches!(
            error,
            WorkerError::ExchangeTimedOut {
                request: "initialize",
                phase: ExchangePhase::Send,
                bound,
            } if bound == SCRIPTED_DEADLINES.send
        ),
        "{error:?}"
    );

    // The peer reads what was written, then the end of the connection.
    peer.until_closed().await;
    // Later exchanges name the deadline that closed the connection.
    let later = worker.inspect().await;
    assert!(
        matches!(
            later,
            Err(WorkerError::ExchangeTimedOut {
                request: "initialize",
                phase: ExchangePhase::Send,
                ..
            })
        ),
        "{later:?}"
    );
}

/// A worker that reads a request and never answers ends the exchange at the
/// answer deadline and sees the connection close.
#[tokio::test(start_paused = true)]
async fn a_worker_that_never_answers_ends_the_exchange_and_the_connection() {
    let (_env, _listener, worker, mut peer) = scripted_pair().await;

    let inspecting = tokio::spawn({
        let worker = worker.clone();
        async move { worker.inspect().await }
    });
    let request = peer.request().await;
    assert!(matches!(request.kind, RequestKind::Inspect { .. }));

    let error = inspecting
        .await
        .expect("the inspection task joins")
        .expect_err("an unanswered inspection ends at its deadline");
    assert!(
        matches!(
            error,
            WorkerError::ExchangeTimedOut {
                request: "inspect",
                phase: ExchangePhase::Answer,
                bound,
            } if bound == SCRIPTED_DEADLINES.answer
        ),
        "{error:?}"
    );
    peer.until_closed().await;
}

/// A stop on a connection an earlier exchange's deadline closed is refused as
/// torn: it is not an unanswered stop, so nothing escalates to the supervisor.
#[tokio::test(start_paused = true)]
async fn a_stop_on_a_connection_closed_by_an_earlier_deadline_is_a_torn_stream() {
    let (_env, _listener, worker, mut peer) = scripted_pair().await;
    let inspecting = tokio::spawn({
        let worker = worker.clone();
        async move { worker.inspect().await }
    });
    peer.request().await;
    inspecting
        .await
        .expect("the inspection task joins")
        .expect_err("an unanswered inspection ends at its deadline");

    let stop = worker
        .stop_within(
            TransactionId::new("stop-after-tear").expect("transaction id"),
            pohunek_daemon::runtime::StopWaits {
                exchange: SCRIPTED_DEADLINES.send,
                input: SCRIPTED_DEADLINES.send,
                answer: SCRIPTED_DEADLINES.answer,
            },
        )
        .await;
    assert!(matches!(stop, Err(WorkerError::TornStream)), "{stop:?}");
}

/// An input write is not cut short by the answer deadline: the worker's own
/// stall policy bounds it, so the connection stays up until the worker says.
#[tokio::test(start_paused = true)]
async fn an_input_answer_outlasts_the_answer_deadline() {
    let (_env, _listener, worker, mut peer) = scripted_pair().await;

    let writing = tokio::spawn({
        let worker = worker.clone();
        async move { worker.write(input_of(16)).await }
    });
    let request = peer.request().await;
    assert!(matches!(request.kind, RequestKind::WritePlan { .. }));

    // The answer deadline passes many times over on the paused clock.
    tokio::time::sleep(SCRIPTED_DEADLINES.answer * 10).await;
    assert!(!writing.is_finished(), "the input keeps waiting");

    peer.respond(request.request_id, rejection()).await;
    let error = writing
        .await
        .expect("the input task joins")
        .expect_err("the scripted worker rejects the input");
    assert!(matches!(error, WorkerError::Rejected { .. }), "{error:?}");
}

/// A worker that answers a request nobody sent ends the connection instead of
/// blocking the reads behind a message nobody waits for.
#[tokio::test(start_paused = true)]
async fn an_unsolicited_answer_closes_the_connection() {
    let (_env, _listener, worker, mut peer) = scripted_pair().await;

    let inspecting = tokio::spawn({
        let worker = worker.clone();
        async move { worker.inspect().await }
    });
    let request = peer.request().await;
    // Both answers reach the daemon before it reads either one.
    peer.respond(request.request_id.clone(), rejection()).await;
    peer.respond(request.request_id, rejection()).await;

    let first = inspecting.await.expect("the inspection task joins");
    assert!(
        matches!(first, Err(WorkerError::Rejected { .. })),
        "{first:?}"
    );
    peer.until_closed().await;
    let later = worker.inspect().await;
    assert!(later.is_err(), "{later:?}");
}

/// An answer to another request is a mismatch.
#[tokio::test(start_paused = true)]
async fn an_answer_to_another_request_is_a_mismatch() {
    let (_env, _listener, worker, mut peer) = scripted_pair().await;

    let inspecting = tokio::spawn({
        let worker = worker.clone();
        async move { worker.inspect().await }
    });
    let _request = peer.request().await;
    peer.respond(
        RequestId::new("another-request").expect("request id"),
        rejection(),
    )
    .await;

    let error = inspecting
        .await
        .expect("the inspection task joins")
        .expect_err("a foreign answer is refused");
    assert!(matches!(error, WorkerError::ResponseMismatch), "{error:?}");
}

/// Dropping the controller ends the connection, so the worker can grant its
/// lease to the next daemon at once.
#[tokio::test]
async fn dropping_the_controller_releases_the_lease() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let served = Served::start(None).await;
    let first = Worker::connect(&served.socket, SESSION_ID, DAEMON_ID)
        .await
        .expect("first controller");
    drop(first);

    wait_until("the lease to be released", || async {
        Worker::connect(&served.socket, SESSION_ID, DAEMON_ID)
            .await
            .ok()
    })
    .await;
}

/// A raw controller that holds the lease of an initialized runtime.
struct Raw {
    reader: ControlReader<OwnedReadHalf>,
    writer: ControlWriter<OwnedWriteHalf>,
    lease_id: LeaseId,
    instance: WorkerInstanceId,
    /// The newest `OutputAdvanced` offset read from the connection.
    last_offset: u64,
}

impl Raw {
    /// Connects with small buffers and initializes the served child.
    async fn start(served: &Served) -> Self {
        let stream = UnixStream::connect(&served.socket).await.expect("connect");
        let socket = socket2::SockRef::from(&stream);
        socket
            .set_send_buffer_size(SOCKET_BUFFER_BYTES)
            .and_then(|()| socket.set_recv_buffer_size(SOCKET_BUFFER_BYTES))
            .expect("shrink the raw client buffers");
        let (read_half, write_half) = stream.into_split();
        let mut reader = ControlReader::new(read_half);
        let mut writer = ControlWriter::new(write_half);
        let daemon = DaemonId::new(DAEMON_ID).expect("daemon id");
        send(
            &mut writer,
            "raw-negotiate",
            RequestKind::Negotiate {
                daemon_instance_id: daemon.clone(),
                minimum_version: SUPPORTED_RANGE.minimum(),
                maximum_version: SUPPORTED_RANGE.maximum(),
            },
        )
        .await;
        let ResponseKind::Negotiated { challenge, .. } = answer(&mut reader).await else {
            panic!("expected a negotiation");
        };
        send(
            &mut writer,
            "raw-acquire",
            RequestKind::AcquireController {
                daemon_instance_id: daemon,
                challenge,
                requested_capabilities: Vec::new(),
            },
        )
        .await;
        let ResponseKind::ControllerAcquired { lease_id, .. } = answer(&mut reader).await else {
            panic!("expected a lease");
        };
        send(
            &mut writer,
            "raw-initialize",
            RequestKind::Initialize {
                lease_id: lease_id.clone(),
                initialize: Box::new(served.child()),
            },
        )
        .await;
        let ResponseKind::Initialized {
            worker_instance_id, ..
        } = answer(&mut reader).await
        else {
            panic!("expected an initialized runtime");
        };
        Self {
            reader,
            writer,
            lease_id,
            instance: worker_instance_id,
            last_offset: 0,
        }
    }

    /// Reads whole lines until the response to `request_id`, noting output
    /// watermarks on the way.
    async fn response_to(&mut self, request_id: &str) -> ResponseKind {
        loop {
            match guard("a control line", self.reader.read::<ControlMessage>())
                .await
                .expect("every line is whole JSON")
                .expect("the worker keeps the connection")
            {
                ControlMessage::Event(event) => {
                    if let EventKind::OutputAdvanced { next_offset, .. } = event.kind {
                        self.last_offset = next_offset;
                    }
                }
                ControlMessage::Response(response) => {
                    assert_eq!(response.request_id.as_str(), request_id);
                    return response.kind;
                }
                ControlMessage::Request(_) => panic!("the worker sent a request"),
            }
        }
    }
}

/// A controller that reads the connection only inside an exchange cannot
/// wedge the worker: while an event line is blocked the
/// worker still reads the request, keeps events and answers in whole lines,
/// and ends the pending line before a release.
#[tokio::test]
async fn a_peer_that_reads_only_inside_an_exchange_cannot_wedge_the_worker() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let served = Served::start(Some(SOCKET_BUFFER_BYTES)).await;
    let mut raw = Raw::start(&served).await;

    // The child prints while this client reads nothing: the worker's event
    // line blocks on the full socket.
    served.printed().await;

    // A request larger than both buffers is read by the worker anyway; the
    // runtime is initialized, so the worker rejects it after reading it whole.
    let mut padded = initialize(&served.root, vec!["x".repeat(OVERSIZED_ARGUMENT_BYTES)]);
    padded.transaction_id = TransactionId::new("raw-rejected").expect("transaction id");
    send(
        &mut raw.writer,
        "raw-oversized",
        RequestKind::Initialize {
            lease_id: raw.lease_id.clone(),
            initialize: Box::new(padded),
        },
    )
    .await;
    let rejected = raw.response_to("raw-oversized").await;
    assert!(
        matches!(rejected, ResponseKind::Error { .. }),
        "{rejected:?}"
    );

    // Once the backlog drained, the newest watermark is not past the runtime's.
    send(
        &mut raw.writer,
        "raw-final-inspect",
        RequestKind::Inspect {
            lease_id: raw.lease_id.clone(),
        },
    )
    .await;
    let ResponseKind::Inspected { snapshot } = raw.response_to("raw-final-inspect").await else {
        panic!("expected an inspection");
    };
    assert_eq!(snapshot.worker_instance_id.as_ref(), Some(&raw.instance));
    assert!(raw.last_offset <= snapshot.next_offset);

    // Releasing the lease completes the pending line, answers, and ends.
    send(
        &mut raw.writer,
        "raw-release",
        RequestKind::ReleaseController {
            lease_id: raw.lease_id.clone(),
        },
    )
    .await;
    let released = raw.response_to("raw-release").await;
    assert!(matches!(released, ResponseKind::ControllerReleased));
    let closed = guard(
        "the end of the connection",
        raw.reader.read::<ControlMessage>(),
    )
    .await
    .expect("every line is whole JSON");
    assert!(closed.is_none(), "{closed:?}");
}

async fn send(writer: &mut ControlWriter<OwnedWriteHalf>, request_id: &str, kind: RequestKind) {
    let request_id = RequestId::new(request_id).expect("request id");
    writer
        .write(&ControlMessage::Request(ControlRequest {
            request_id,
            kind,
        }))
        .await
        .expect("write a request");
    writer.flush().await.expect("flush a request");
}

/// The next response, skipping events.
async fn answer(reader: &mut ControlReader<OwnedReadHalf>) -> ResponseKind {
    loop {
        match guard("a control line", reader.read::<ControlMessage>())
            .await
            .expect("every line is whole JSON")
            .expect("the worker keeps the connection")
        {
            ControlMessage::Response(response) => return response.kind,
            ControlMessage::Event(_) => {}
            ControlMessage::Request(_) => panic!("the worker sent a request"),
        }
    }
}

/// A controller that vanishes while the worker has an event line pending
/// releases the lease: the worker drops the partial line and the next
/// controller connects.
#[tokio::test]
async fn a_controller_that_vanishes_during_an_event_backlog_releases_the_lease() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let served = Served::start(Some(SOCKET_BUFFER_BYTES)).await;
    let raw = Raw::start(&served).await;
    served.printed().await;
    drop(raw);

    wait_until("the lease to be released", || async {
        Worker::connect(&served.socket, SESSION_ID, DAEMON_ID)
            .await
            .ok()
    })
    .await;
}
