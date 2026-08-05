//! End-to-end privacy sentinel test (Durable Session Workers RFC, Definition
//! of Done item 4: `docs/design/durable-session-workers-rfc.md` "Required
//! work before merge").
//!
//! Drives one real `pohunek-sessiond`-backed session with a distinct,
//! searchable sentinel in every channel the RFC's privacy model draws a line
//! around — prompt, argv, environment, terminal output, and a reported native
//! id ("hook value") — then scans every durable artifact the daemon and
//! worker produce: the logical store (`metadata.jsonl`), every worker
//! journal, the append-only event log, the worker's own structured log, and
//! the daemon's own structured log. Only the sentinels the design explicitly
//! permits (RFC §10.2 "launch stores the existing structural snapshot... it
//! never stores profile environment variables or initial input"; RFC §10.1
//! "immutable launch native reference, when captured") may appear, and only
//! in the artifact the design permits them in; everything else must be
//! absent everywhere (RFC §22 "structured logs never include environment
//! values, prompt/input bytes, terminal bytes, data tokens, controller
//! tokens, or native reference values").

// Rust guideline compliant 2026-07-24

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use protocol::{
    method, AttachHeader, ProcessStartIdentity, ReportSequence, Request as ProtocolRequest,
    Response, SessionAttachParams, SessionAttachResult, SessionInfo, SessionNewParams,
    SessionReportNativeIdParams, SessionReportNativeIdResult, SessionState, SessionStopResult,
};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio_util::codec::{Framed, LinesCodec};

use pohunek_daemon::api::{ControlServer, DaemonState, HealthInfo};
use pohunek_daemon::events::{spawn_drain, EventLog};
use pohunek_daemon::procwatch::LinuxInspector;
use pohunek_daemon::runtime::{SubprocessWorkerEnvironment, SubprocessWorkerLauncher};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct Request;

impl Request {
    fn make(id: &str, method: &str, params: serde_json::Value) -> ProtocolRequest {
        ProtocolRequest::new(id, method, params).expect("valid integration-test request")
    }
}

/// A unique temp directory for one test, so parallel `#[tokio::test]` runs
/// (and repeated runs of this file) never collide on disk.
fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "pohunek-test-{tag}-{}-{nanos}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

/// Write `body` to `path` and mark it executable, for the stub sentinel agent.
fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write executable test script");
    let mut permissions = std::fs::metadata(path)
        .expect("test script metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("chmod test script");
}

/// Resolve the real `pohunek-sessiond` binary, mirroring
/// `health_socket.rs::worker_binary` — this test drives an actual worker
/// process, not a fake, so the privacy scan covers real production code.
fn worker_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("POHUNEK_WORKER_BIN") {
        return PathBuf::from(path);
    }
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("daemon crate is inside workspace")
        .to_path_buf();
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| workspace.join("target"), PathBuf::from);
    let binary = target.join("debug/pohunek-sessiond");
    assert!(
        binary.is_file(),
        "build the real worker first with `cargo build -p pohunek-session-worker --bin pohunek-sessiond`, or set POHUNEK_WORKER_BIN"
    );
    binary
}

/// Connect a line-framed client to the daemon's control socket.
async fn connect(socket: &Path) -> Framed<UnixStream, LinesCodec> {
    for _ in 0..50 {
        if let Ok(stream) = UnixStream::connect(socket).await {
            return Framed::new(stream, LinesCodec::new());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("could not connect to test socket {}", socket.display());
}

/// Send one request line and read the matching response line.
async fn exchange(
    framed: &mut Framed<UnixStream, LinesCodec>,
    request: &ProtocolRequest,
) -> Response {
    let line = serde_json::to_string(request).expect("serialize request");
    framed.send(line).await.expect("send");
    let reply = framed
        .next()
        .await
        .expect("a response line")
        .expect("response framing ok");
    serde_json::from_str(&reply).expect("parse response")
}

fn ok_payload(response: Response) -> serde_json::Value {
    response
        .into_result()
        .unwrap_or_else(|err| panic!("expected ok, got error: {err}"))
}

fn process_start_identity(pid: u32) -> ProcessStartIdentity {
    let stat =
        std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("read child process identity");
    let fields = stat
        .rsplit_once(") ")
        .expect("process stat contains command terminator")
        .1
        .split_ascii_whitespace()
        .collect::<Vec<_>>();
    let start_identity = fields
        .get(19)
        .expect("process stat contains start identity")
        .parse()
        .expect("process start identity is numeric");
    ProcessStartIdentity::new(start_identity)
}

/// Open a raw attach stream: a fresh connection carrying only the attach
/// header, after which the socket is a pure byte pipe to the PTY.
async fn open_attach_stream(socket: &Path, stream_id: &str) -> UnixStream {
    let mut raw = UnixStream::connect(socket)
        .await
        .expect("connect raw attach stream");
    let header = serde_json::to_string(&AttachHeader {
        attach: stream_id.to_owned(),
    })
    .expect("serialize attach header");
    raw.write_all(header.as_bytes())
        .await
        .expect("send attach header");
    raw.write_all(b"\n").await.expect("terminate attach header");
    raw
}

/// Read from `stream` into one cumulative buffer until every marker in
/// `markers` has appeared as a contiguous byte sequence, proving the
/// sentinels really flowed through the PTY before we assert they never land
/// anywhere durable. A single read syscall can (and here, does) deliver both
/// the child's immediate output and its echo of the injected prompt in one
/// chunk, so checking markers one read-loop at a time — instead of
/// accumulating across all of them — would silently drop whichever marker
/// arrived alongside the first.
async fn read_until_markers(stream: &mut UnixStream, markers: &[&[u8]]) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut collected = Vec::new();
        let mut buf = [0_u8; 1024];
        loop {
            if markers.iter().all(|marker| {
                collected
                    .windows(marker.len())
                    .any(|window| window == *marker)
            }) {
                return collected;
            }
            let n = stream.read(&mut buf).await.expect("read raw stream");
            assert_ne!(n, 0, "raw stream closed before every marker arrived");
            collected.extend_from_slice(&buf[..n]);
        }
    })
    .await
    .expect("every marker arrives before timeout")
}

/// Concatenates every regular file under `dir` (recursively), sorted by path
/// for determinism. A missing directory yields an empty scan, not an error,
/// since some artifacts (e.g. the worker's own log dir) only exist once the
/// worker has actually started.
fn collect_dir_bytes(dir: &Path) -> Vec<u8> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            out.extend(collect_dir_bytes(&path));
        } else if let Ok(bytes) = std::fs::read(&path) {
            out.extend(bytes);
            out.push(b'\n');
        }
    }
    out
}

/// Polls `collect_dir_bytes(dir)` until it contains `marker`, so the scan
/// below never races the worker's own (separate-process, separately-flushed)
/// journal and log writers.
async fn wait_for_dir_containing(dir: &Path, marker: &[u8]) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let bytes = collect_dir_bytes(dir);
            if bytes.windows(marker.len()).any(|window| window == marker) {
                return bytes;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "{} never contained marker {}",
            dir.display(),
            String::from_utf8_lossy(marker)
        )
    })
}

/// Spawn the control server with a worker-backed [`SessionRegistry`] talking
/// to the real `pohunek-sessiond` binary, mirroring
/// `health_socket.rs::spawn_server_with_config`. Notifications are left
/// unwired since this scan only exercises session lifecycle methods. Returns
/// the worker's own XDG home (short-named, per the socket-path-length
/// comment below) so the caller can scan the worker's journal and structured
/// log directly.
async fn spawn_worker_backed_server(
    socket: &Path,
    version: &str,
    mut config: SessionRegistryConfig,
) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>, PathBuf) {
    let event_log_dir = config.event_log_dir.clone();
    // Short prefix: the worker's own control socket nests several directories
    // below this home (`<runtime_home>/pohunek/workers/<session>/control.sock`),
    // and `AF_UNIX` paths are capped at ~108 bytes.
    let worker_home = std::env::temp_dir().join(format!(
        "pw-h-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let worker_environment = SubprocessWorkerEnvironment {
        runtime_home: worker_home.join("runtime"),
        state_home: worker_home.join("state"),
        data_home: worker_home.join("data"),
        config_home: worker_home.join("config"),
        cache_home: worker_home.join("cache"),
        daemon_socket: socket.to_path_buf(),
    };
    config.socket_path = Some(socket.to_path_buf());
    config.worker_runtime_root = Some(worker_environment.runtime_home.join("pohunek/workers"));
    config.worker_state_root = Some(worker_environment.state_home.join("pohunek/workers"));
    let launcher = Arc::new(SubprocessWorkerLauncher::new(
        worker_binary(),
        worker_environment,
    ));
    let registry = SessionRegistry::new_with_launcher_and_inspector(
        config,
        launcher,
        Arc::new(LinuxInspector::new()),
    );
    if let Some(event_log_dir) = event_log_dir {
        let log = Arc::new(EventLog::open(&event_log_dir).expect("event log opens"));
        let _drain = spawn_drain(
            log,
            registry.subscribe(),
            tokio_util::sync::CancellationToken::default(),
        );
    }
    let state = DaemonState::new(HealthInfo::new(version), registry);
    let server = ControlServer::bind_with_state(socket, state)
        .await
        .expect("server binds");
    let (tx, rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        server
            .serve(async move {
                let _ = rx.await;
            })
            .await;
    });
    (tx, handle, worker_home)
}

/// Milestone: Durable Session Workers RFC, Definition of Done item 4.
///
/// Creates one worker-backed session whose launch carries a distinct sentinel
/// in the prompt, argv, environment, terminal output, and a reported native
/// id, then scans the logical store, every worker journal, the event log, the
/// worker's own structured log, and the daemon's own structured log. Asserts
/// the allow/deny matrix from RFC §10.1 (worker journal), §10.2 (logical
/// store), and §22 (structured logs):
///
/// - prompt/input bytes, raw terminal output, and environment VALUES must
///   never appear in any artifact;
/// - argv is a structural launch detail the logical store legitimately
///   freezes (§10.2 "launch stores the existing structural snapshot...
///   program, arguments..."), but it must never reach the worker journal
///   (§10.1 lists no such content) or any structured log;
/// - the reported native id is the native-recovery reference the store keeps
///   (§10.2), and the event log legitimately mirrors it too (it only ever
///   echoes the wire-facing `SessionInfo` clients already see via
///   `session.inspect`/`session.list`), but §22 forbids it from ever reaching
///   a structured log, and — since this test reports it over the public
///   daemon RPC rather than the worker's own private hook socket — it never
///   reaches the worker journal captured by that path either.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one end-to-end privacy scan intentionally drives the full session \
              lifecycle across five distinct on-disk artifacts"
)]
async fn worker_backed_session_never_persists_secrets_or_terminal_bytes() {
    const PROMPT: &str = "SENTINEL_PROMPT_9f3a";
    const ARGV: &str = "SENTINEL_ARGV_9f3a";
    const ENV_VALUE: &str = "SENTINEL_ENV_9f3a";
    const OUTPUT: &str = "SENTINEL_OUTPUT_9f3a";
    const HOOK: &str = "SENTINEL_HOOK_9f3a";

    let root = temp_dir("privacy-sentinel");
    let data_dir = root.join("data");
    let store_path = data_dir.join("metadata.jsonl");
    let events_dir = data_dir.join("events");
    let agents_dir = root.join("agents");
    let daemon_log_dir = root.join("daemon-logs");
    std::fs::create_dir_all(&agents_dir).expect("create agents dir");

    // Stub sentinel agent: prints a terminal-output sentinel unrelated to any
    // input, then echoes back whatever the daemon injects as the initial
    // prompt, exactly as a real agent's terminal echo would. The managed
    // child's environment carries only the profile's `env` plus the reserved
    // `POHUNEK_*` handshake keys (RFC §22) — no `PATH` — so `cat` is invoked
    // by absolute path rather than relying on `execvp` path resolution.
    let script = agents_dir.join("stub-agent.sh");
    write_executable(
        &script,
        &format!("#!/bin/sh\nset -eu\nprintf '{OUTPUT}\\n'\nexec /bin/cat\n"),
    );
    // A host agent profile (Part C): `base = "claude"` makes the session
    // resumable (required for `session.report_native_id` below) while
    // `program`/`args` override the launch entirely with our stub. `env` is a
    // non-`POHUNEK_`-prefixed key so the loader does not strip it.
    std::fs::write(
        agents_dir.join("sentinelclaude.toml"),
        format!(
            "base = \"claude\"\nprogram = \"{}\"\nargs = [\"--sentinel-argv\", \"{ARGV}\"]\n\n[env]\nAPI_TOKEN = \"{ENV_VALUE}\"\n",
            script.display()
        ),
    )
    .expect("write sentinel agent profile");

    // Install the daemon's real JSON logging sink so this test scans the
    // exact structured-log path production uses (RFC §22/§23), not a stand-in.
    let log_guard =
        pohunek_daemon::logging::init(&daemon_log_dir).expect("daemon logging initializes");

    let socket = temp_dir("privacy-sentinel-sock").join("daemon.sock");
    let config = SessionRegistryConfig {
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        event_log_dir: Some(events_dir.clone()),
        agents_dir: Some(agents_dir.clone()),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, worker_home) =
        spawn_worker_backed_server(&socket, "0.0.0", config).await;
    let worker_state_root = worker_home.join("state/pohunek/workers");
    let worker_log_dir = worker_home.join("state/pohunek/logs");

    let mut control = connect(&socket).await;

    let create_req = Request::make(
        "privacy-sentinel-create",
        method::SESSION_NEW,
        serde_json::to_value(SessionNewParams {
            name: None,
            agent: "sentinelclaude".to_owned(),
            cwd: Some(std::env::temp_dir()),
            cols: 80,
            rows: 24,
            project: None,
            repo: None,
            branch: None,
            base_branch: None,
            input: Some(PROMPT.to_owned()),
            metadata: BTreeMap::new(),
        })
        .expect("serialize session.new params"),
    );
    let created: SessionInfo =
        serde_json::from_value(ok_payload(exchange(&mut control, &create_req).await))
            .expect("sentinel session info");
    assert_eq!(created.state, SessionState::Running);

    // Wait for the worker to have durably written its journal at least once
    // before driving the rest of the scenario.
    let _ = wait_for_dir_containing(&worker_state_root, created.id.0.as_bytes()).await;

    // Attach and read the child's real terminal output: proof the output and
    // prompt sentinels actually flowed through the PTY, before asserting they
    // never land anywhere durable below.
    let attach_req = Request::make(
        "privacy-sentinel-attach",
        method::SESSION_ATTACH,
        serde_json::to_value(SessionAttachParams {
            session_id: created.id.clone(),
            initial_dimensions: None,
            origin_session_id: None,
            origin_daemon_id: None,
            origin_worker_id: None,
        })
        .expect("serialize attach params"),
    );
    let attach: SessionAttachResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &attach_req).await))
            .expect("attach result");
    let mut raw = open_attach_stream(&socket, &attach.stream_id).await;
    let terminal_seen = read_until_markers(&mut raw, &[OUTPUT.as_bytes(), PROMPT.as_bytes()]).await;
    assert!(
        terminal_seen
            .windows(OUTPUT.len())
            .any(|w| w == OUTPUT.as_bytes()),
        "the stub agent's terminal output sentinel must reach the attach stream"
    );
    assert!(
        terminal_seen
            .windows(PROMPT.len())
            .any(|w| w == PROMPT.as_bytes()),
        "the injected prompt sentinel must reach the attach stream via the agent's echo"
    );
    drop(raw);

    // Report a native id — the "hook value" channel — over the public
    // `session.report_native_id` RPC, exactly as
    // `report_native_id_records_binding_and_updates_info` in
    // `session/tests.rs` does; this is the same call the shipped
    // `pohunek-agent-state.sh` hook falls back to when it cannot reach the
    // worker's private hook socket.
    let runtime_id = created
        .runtime
        .as_ref()
        .and_then(|runtime| runtime.runtime_id.as_deref())
        .expect("created managed session exposes its runtime id");
    let report_req = Request::make(
        "privacy-sentinel-report-native-id",
        method::SESSION_REPORT_NATIVE_ID,
        serde_json::to_value(
            SessionReportNativeIdParams::new(
                created.id.clone(),
                runtime_id,
                "claude",
                created.pid,
                process_start_identity(created.pid),
                ReportSequence::new(1),
                (OffsetDateTime::now_utc() + time::Duration::seconds(30))
                    .format(&Rfc3339)
                    .expect("format native identity expiry"),
                HOOK,
                None,
            )
            .expect("valid native identity claim"),
        )
        .expect("serialize report-native-id params"),
    );
    let reported: SessionReportNativeIdResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &report_req).await))
            .expect("report-native-id result");
    assert!(reported.recorded, "the sentinel native id must be recorded");

    let stop_req = Request::make(
        "privacy-sentinel-stop",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let _: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &stop_req).await))
            .expect("stop result");

    // Collect the worker's own durable artifacts before tearing the server
    // down; the session id is always present (a safe identifier), so it also
    // doubles as a readiness marker against the worker's own async writers.
    let journal_bytes = wait_for_dir_containing(&worker_state_root, created.id.0.as_bytes()).await;
    let worker_log_bytes = wait_for_dir_containing(&worker_log_dir, created.id.0.as_bytes()).await;
    let event_log_bytes = wait_for_dir_containing(&events_dir, created.id.0.as_bytes()).await;

    let _ = shutdown.send(());
    let _ = handle.await;
    // The daemon's non-blocking JSON writer flushes synchronously on guard
    // drop, so the daemon log is read directly rather than polled.
    drop(log_guard);

    let store_bytes = std::fs::read(&store_path).expect("read logical store");
    let daemon_log_bytes = collect_dir_bytes(&daemon_log_dir);

    let artifacts: [(&str, &[u8]); 5] = [
        ("logical store", &store_bytes),
        ("worker journal", &journal_bytes),
        ("event log", &event_log_bytes),
        ("worker structured log", &worker_log_bytes),
        ("daemon structured log", &daemon_log_bytes),
    ];

    // Prompt/input bytes, raw terminal output, and environment values must
    // never appear in ANY artifact (RFC §10.1, §10.2, §22).
    let forbidden_everywhere = [
        (PROMPT, "the initial prompt/input bytes"),
        (OUTPUT, "raw terminal output bytes"),
        (ENV_VALUE, "the profile environment value"),
    ];
    for (artifact_name, bytes) in artifacts {
        let text = String::from_utf8_lossy(bytes);
        for (sentinel, what) in forbidden_everywhere {
            assert!(
                !text.contains(sentinel),
                "{artifact_name} must never contain {what} ({sentinel}): {text}"
            );
        }
    }

    // Argv is a structural launch detail: the logical store's launch snapshot
    // legitimately freezes program/args (RFC §10.2), but no other artifact may
    // carry it — the worker journal's documented contents (RFC §10.1) name no
    // such field, the event log only ever mirrors the wire-facing
    // `SessionInfo` (which has no program/args field), and it must never
    // reach a structured log.
    assert_allowed_only_in(&artifacts, ARGV, "the launch argv", &["logical store"]);

    // The reported native id is the native-recovery reference `SessionInfo`
    // exposes to clients (RFC §10.2 "native_recovery"), so it legitimately
    // appears both in the logical store AND the event log, which mirrors that
    // same client-facing snapshot verbatim (matching the pre-existing
    // `event_log_records_lifecycle_and_never_terminal_bytes` precedent, which
    // only forbids raw terminal bytes from the event log — not native ids).
    // But RFC §22 is explicit that a *structured log* never includes native
    // reference values, and this test's public-RPC report path never reaches
    // the worker's own private-hook-socket-fed journal either.
    assert_allowed_only_in(
        &artifacts,
        HOOK,
        "the reported native id",
        &["logical store", "event log"],
    );

    // A safe identifier (the session id) legitimately appears everywhere.
    let store_text = String::from_utf8_lossy(&store_bytes);
    assert!(
        store_text.contains(&created.id.0),
        "the logical store must record its own session id"
    );
}

/// Asserts `sentinel` appears in every artifact named in `allowed_in` and in
/// none of the others — the allow/deny half of the privacy matrix for a
/// sentinel that is legitimately persisted somewhere.
fn assert_allowed_only_in(
    artifacts: &[(&str, &[u8])],
    sentinel: &str,
    what: &str,
    allowed_in: &[&str],
) {
    for (artifact_name, bytes) in artifacts {
        let text = String::from_utf8_lossy(bytes);
        let allowed = allowed_in.contains(artifact_name);
        assert_eq!(
            text.contains(sentinel),
            allowed,
            "{artifact_name} {} contain {what} ({sentinel}): {text}",
            if allowed { "must" } else { "must never" }
        );
    }
}
