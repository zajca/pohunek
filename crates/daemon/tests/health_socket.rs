//! Integration test: the daemon's control server answers `daemon.health` over a
//! real Unix socket using newline-delimited JSON.
//!
//! This is the milestone-2 checkpoint ("CLI `doctor` + `daemon start` talk over
//! the socket") exercised at the protocol layer: it binds the actual
//! `ControlServer` on a temp socket, connects a raw client, and verifies the
//! response carries the daemon and protocol versions. It also covers
//! stale-socket recovery and the `method_not_found` path.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use protocol::{method, Request, Response, PROTOCOL_VERSION};
use serde_json::Value;
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio_util::codec::{Framed, LinesCodec};

use zagentmesh_daemon::api::{ControlServer, HealthInfo};

/// A unique temp socket path inside a dedicated per-test directory.
///
/// The server enforces the directory's mode on bind, so the socket must live in
/// a directory we own (not `/tmp` itself, which is root-owned with the sticky
/// bit). This mirrors the real daemon, which always binds inside its own
/// `zagentmesh` runtime subdir.
fn temp_socket(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "zagentmesh-test-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create test socket dir");
    dir.join("daemon.sock")
}

/// Spawn the control server on `socket`, returning a shutdown trigger and the
/// server task handle.
async fn spawn_server(
    socket: &std::path::Path,
    version: &str,
) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let health = HealthInfo::new(version);
    let server = ControlServer::bind(socket, health)
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
    (tx, handle)
}

/// Connect a raw line-framed client to `socket`.
async fn connect(socket: &std::path::Path) -> Framed<UnixStream, LinesCodec> {
    // Brief retry: bind returns before the listener is necessarily accepting in
    // all timing scenarios.
    for _ in 0..50 {
        if let Ok(stream) = UnixStream::connect(socket).await {
            return Framed::new(stream, LinesCodec::new());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("could not connect to test socket {}", socket.display());
}

/// Send a request line and read one response line.
async fn exchange(framed: &mut Framed<UnixStream, LinesCodec>, request: &Request) -> Response {
    let line = serde_json::to_string(request).expect("serialize request");
    framed.send(line).await.expect("send");
    let reply = framed
        .next()
        .await
        .expect("a response line")
        .expect("response framing ok");
    serde_json::from_str(&reply).expect("parse response")
}

#[tokio::test]
async fn health_returns_versions() {
    let socket = temp_socket("health");
    let (shutdown, handle) = spawn_server(&socket, "9.9.9-test").await;

    let mut client = connect(&socket).await;
    let req = Request::new("t-1", method::DAEMON_HEALTH, Value::Null);
    let resp = exchange(&mut client, &req).await;

    match resp {
        Response::Ok { v, id, ok } => {
            assert_eq!(v, PROTOCOL_VERSION);
            assert_eq!(id, "t-1");
            assert_eq!(ok["status"], Value::from("ok"));
            assert_eq!(ok["daemon_version"], Value::from("9.9.9-test"));
            assert_eq!(ok["protocol_version"], Value::from(PROTOCOL_VERSION.get()));
        }
        Response::Err { err, .. } => panic!("expected ok, got error: {err}"),
    }

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn unknown_method_returns_typed_error() {
    let socket = temp_socket("unknown");
    let (shutdown, handle) = spawn_server(&socket, "0.0.0").await;

    let mut client = connect(&socket).await;
    let req = Request::new("t-2", "no.such.method", Value::Null);
    let resp = exchange(&mut client, &req).await;

    match resp {
        Response::Err { id, err, .. } => {
            assert_eq!(id, "t-2");
            assert_eq!(err.code, "method_not_found");
        }
        Response::Ok { .. } => panic!("expected an error for an unknown method"),
    }

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn stale_socket_is_recovered_on_bind() {
    let socket = temp_socket("stale");
    // Create a stale socket file with no listener behind it.
    {
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind stale");
        drop(listener);
    }
    assert!(socket.exists(), "stale socket file should exist");

    // Binding again must succeed by removing the stale socket.
    let (shutdown, handle) = spawn_server(&socket, "0.0.0").await;

    let mut client = connect(&socket).await;
    let req = Request::new("t-3", method::DAEMON_HEALTH, Value::Null);
    let resp = exchange(&mut client, &req).await;
    assert!(matches!(resp, Response::Ok { .. }), "health works after recovery");

    let _ = shutdown.send(());
    let _ = handle.await;
}
