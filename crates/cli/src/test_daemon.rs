//! Fixture daemon for in-crate lifecycle scenarios.
//!
//! It answers `daemon.health` on every connection and holds the reply to the
//! first other request until the test releases it, so a scenario on a paused
//! clock decides when the daemon answers.

use pohunek_test_support::env::TestEnv;
use protocol::{Request, Response, PROTOCOL_VERSION};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::paths::Paths;

/// A lifecycle request the fixture received, and whether it travelled on the
/// first connection the client opened (the shared connection).
#[derive(Debug)]
pub(crate) struct Received {
    pub(crate) request: Request,
    pub(crate) on_shared_connection: bool,
}

/// A fixture daemon holding the reply to its first lifecycle request.
pub(crate) struct HeldDaemon {
    pub(crate) paths: Paths,
    received: mpsc::UnboundedReceiver<Received>,
    release: Option<oneshot::Sender<()>>,
    accept: JoinHandle<()>,
    _env: TestEnv,
}

impl HeldDaemon {
    /// Binds the fixture on a private socket; `result` answers the lifecycle
    /// request once released.
    pub(crate) fn start(result: Value) -> Self {
        let env = TestEnv::new().expect("create the hermetic test environment");
        let socket = env
            .socket_path("daemon.sock")
            .expect("socket path fits the sun_path limit");
        let root = socket.parent().expect("socket parent").to_path_buf();
        let paths = Paths {
            runtime_dir: root.clone(),
            socket: socket.clone(),
            data_dir: root.join("data"),
            log_dir: root.join("logs"),
            cache_dir: root.join("cache"),
            config_home: root.join("config-home"),
            config_dir: root.join("config"),
            origin_source: pohunek_client::OriginSource::Omitted,
        };
        // Binding before the command runs is the readiness signal.
        let listener = UnixListener::bind(&socket).expect("bind fixture daemon");
        let (received_tx, received) = mpsc::unbounded_channel();
        let (release, release_rx) = oneshot::channel::<()>();
        let release_rx = std::sync::Arc::new(tokio::sync::Mutex::new(Some(release_rx)));
        let accept = tokio::spawn(async move {
            for connection in 0_usize.. {
                let (stream, _) = listener.accept().await.expect("accept control client");
                tokio::spawn(serve_connection(
                    stream,
                    connection == 0,
                    result.clone(),
                    received_tx.clone(),
                    std::sync::Arc::clone(&release_rx),
                ));
            }
        });
        Self {
            paths,
            received,
            release: Some(release),
            accept,
            _env: env,
        }
    }

    /// Waits for the first lifecycle request.
    pub(crate) async fn request(&mut self) -> Received {
        self.received
            .recv()
            .await
            .expect("the fixture daemon received a lifecycle request")
    }

    /// Lets the daemon answer the held request.
    pub(crate) fn release(&mut self) {
        // A scenario that ended earlier dropped the receiver already.
        let _ = self
            .release
            .take()
            .expect("the reply is released once")
            .send(());
    }
}

impl Drop for HeldDaemon {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

async fn serve_connection(
    stream: UnixStream,
    shared: bool,
    result: Value,
    received: mpsc::UnboundedSender<Received>,
    release: std::sync::Arc<tokio::sync::Mutex<Option<oneshot::Receiver<()>>>>,
) {
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }
        let request: Request = serde_json::from_str(&line).expect("decode request");
        let value = if request.method() == protocol::method::DAEMON_HEALTH {
            json!({
                "status": "ok",
                "daemon_version": "fixture",
                "protocol_version": PROTOCOL_VERSION.get()
            })
        } else {
            let held = release.lock().await.take();
            let id = request.id().to_owned();
            let version = request.version_range().maximum();
            let _ = received.send(Received {
                request,
                on_shared_connection: shared,
            });
            if let Some(held) = held {
                // The client may have given up already; a dropped sender
                // ends the wait as well.
                let _ = held.await;
            }
            let response = Response::ok(version, id, result.clone()).expect("valid response");
            let mut encoded = serde_json::to_vec(&response).expect("encode response");
            encoded.push(b'\n');
            // The client may have closed the connection.
            let _ = stream.get_mut().write_all(&encoded).await;
            continue;
        };
        let response = Response::ok(
            request.version_range().maximum(),
            request.id().to_owned(),
            value,
        )
        .expect("valid response");
        let mut encoded = serde_json::to_vec(&response).expect("encode response");
        encoded.push(b'\n');
        stream
            .get_mut()
            .write_all(&encoded)
            .await
            .expect("write response");
    }
}
