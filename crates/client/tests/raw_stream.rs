use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use pohunek_client::protocol::AttachHeader;
use pohunek_client::{
    attach_raw, attach_raw_local, attach_raw_tcp_addr, connect_raw, connect_raw_local,
    connect_raw_tcp_addr, RawStream,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const HOST: &str = "build-box";

static NEXT_SOCKET_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct UnixRawDaemon {
    socket_path: PathBuf,
    captured: oneshot::Receiver<CapturedRawStream>,
    task: JoinHandle<()>,
    _socket_file: SocketFile,
}

#[derive(Debug)]
struct TcpRawDaemon {
    addr: SocketAddr,
    captured: oneshot::Receiver<CapturedRawStream>,
    task: JoinHandle<()>,
}

#[derive(Debug)]
struct CapturedRawStream {
    header_line: String,
    body: Vec<u8>,
}

#[derive(Debug)]
struct SocketFile(PathBuf);

impl Drop for SocketFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[tokio::test]
async fn raw_stream_connect_raw_local_carries_attach_header_and_unframed_bytes() {
    let daemon = spawn_unix_raw_daemon();
    let body = vec![0x00, b'p', b't', b'y', b'\n', 0xff, b'x'];

    let raw = connect_raw_local(&daemon.socket_path)
        .await
        .expect("connect local raw daemon");
    match raw {
        RawStream::Local(mut stream) => {
            write_attach_stream(&mut stream, "stream-local", &body).await;
        }
        RawStream::Remote(_) => panic!("local raw connection returned remote stream"),
        _ => panic!("local raw connection returned unknown stream"),
    }

    assert_captured(
        &daemon
            .captured
            .await
            .expect("daemon captured local raw stream"),
        "stream-local",
        &body,
    );
    daemon.task.await.expect("raw unix daemon task completed");
}

#[tokio::test]
async fn raw_stream_attach_raw_local_writes_attach_header_before_unframed_bytes() {
    let daemon = spawn_unix_raw_daemon();
    let body = vec![0x00, b'p', b't', b'y', b'\n', 0xff, b'x'];

    let raw = attach_raw_local(&daemon.socket_path, "stream-local")
        .await
        .expect("connect local attach stream");
    match raw {
        RawStream::Local(mut stream) => {
            stream.write_all(&body).await.expect("write raw body");
            stream.shutdown().await.expect("close raw stream");
        }
        RawStream::Remote(_) => panic!("local attach stream returned remote stream"),
        _ => panic!("local attach stream returned unknown stream"),
    }

    assert_captured(
        &daemon
            .captured
            .await
            .expect("daemon captured local attach stream"),
        "stream-local",
        &body,
    );
    daemon.task.await.expect("raw unix daemon task completed");
}

#[tokio::test]
async fn raw_stream_connect_raw_routes_local_host_to_unix_socket() {
    let daemon = spawn_unix_raw_daemon();
    let body = b"local-routing-bytes".to_vec();

    let raw = connect_raw("local", &daemon.socket_path)
        .await
        .expect("connect routed local raw daemon");
    match raw {
        RawStream::Local(mut stream) => {
            write_attach_stream(&mut stream, "stream-routed-local", &body).await;
        }
        RawStream::Remote(_) => panic!("routed local raw connection returned remote stream"),
        _ => panic!("routed local raw connection returned unknown stream"),
    }

    assert_captured(
        &daemon
            .captured
            .await
            .expect("daemon captured routed local raw stream"),
        "stream-routed-local",
        &body,
    );
    daemon.task.await.expect("raw unix daemon task completed");
}

#[tokio::test]
async fn raw_stream_attach_raw_routes_local_host_to_unix_socket_and_writes_attach_header() {
    let daemon = spawn_unix_raw_daemon();
    let body = b"local-routing-bytes".to_vec();

    let raw = attach_raw("local", &daemon.socket_path, "stream-routed-local")
        .await
        .expect("connect routed local attach stream");
    match raw {
        RawStream::Local(mut stream) => {
            stream.write_all(&body).await.expect("write raw body");
            stream.shutdown().await.expect("close raw stream");
        }
        RawStream::Remote(_) => panic!("routed local attach stream returned remote stream"),
        _ => panic!("routed local attach stream returned unknown stream"),
    }

    assert_captured(
        &daemon
            .captured
            .await
            .expect("daemon captured routed local attach stream"),
        "stream-routed-local",
        &body,
    );
    daemon.task.await.expect("raw unix daemon task completed");
}

#[tokio::test]
async fn raw_stream_connect_raw_tcp_addr_carries_attach_header_and_unframed_bytes() {
    let daemon = spawn_tcp_raw_daemon().await;
    let body = vec![b'r', b'e', b'm', b'o', b't', b'e', 0x00, 0xfe, b'\n'];

    let raw = connect_raw_tcp_addr(HOST, daemon.addr)
        .await
        .expect("connect tcp raw daemon");
    match raw {
        RawStream::Remote(mut stream) => {
            write_attach_stream(&mut stream, "stream-remote", &body).await;
        }
        RawStream::Local(_) => panic!("tcp raw connection returned local stream"),
        _ => panic!("tcp raw connection returned unknown stream"),
    }

    assert_captured(
        &daemon
            .captured
            .await
            .expect("daemon captured tcp raw stream"),
        "stream-remote",
        &body,
    );
    daemon.task.await.expect("raw tcp daemon task completed");
}

#[tokio::test]
async fn raw_stream_attach_raw_tcp_addr_writes_attach_header_before_unframed_bytes() {
    let daemon = spawn_tcp_raw_daemon().await;
    let body = vec![b'r', b'e', b'm', b'o', b't', b'e', 0x00, 0xfe, b'\n'];

    let raw = attach_raw_tcp_addr(HOST, daemon.addr, "stream-remote")
        .await
        .expect("connect tcp attach stream");
    match raw {
        RawStream::Remote(mut stream) => {
            stream.write_all(&body).await.expect("write raw body");
            stream.shutdown().await.expect("close raw stream");
        }
        RawStream::Local(_) => panic!("tcp attach stream returned local stream"),
        _ => panic!("tcp attach stream returned unknown stream"),
    }

    assert_captured(
        &daemon
            .captured
            .await
            .expect("daemon captured tcp attach stream"),
        "stream-remote",
        &body,
    );
    daemon.task.await.expect("raw tcp daemon task completed");
}

fn spawn_unix_raw_daemon() -> UnixRawDaemon {
    let socket_path = unique_socket_path();
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).expect("bind unix raw test daemon");
    let (captured_tx, captured) = oneshot::channel();

    let task = tokio::spawn(async move {
        let (stream, _addr) = listener.accept().await.expect("accept unix raw client");
        capture_raw_stream(stream, captured_tx).await;
    });

    UnixRawDaemon {
        socket_path: socket_path.clone(),
        captured,
        task,
        _socket_file: SocketFile(socket_path),
    }
}

async fn spawn_tcp_raw_daemon() -> TcpRawDaemon {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind tcp raw test daemon");
    let addr = listener.local_addr().expect("read tcp raw daemon address");
    let (captured_tx, captured) = oneshot::channel();

    let task = tokio::spawn(async move {
        let (stream, _addr) = listener.accept().await.expect("accept tcp raw client");
        capture_raw_stream(stream, captured_tx).await;
    });

    TcpRawDaemon {
        addr,
        captured,
        task,
    }
}

async fn capture_raw_stream<S>(stream: S, captured_tx: oneshot::Sender<CapturedRawStream>)
where
    S: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut header_line = String::new();
    reader
        .read_line(&mut header_line)
        .await
        .expect("read attach header line");

    let mut body = Vec::new();
    reader
        .read_to_end(&mut body)
        .await
        .expect("read raw attach body");

    captured_tx
        .send(CapturedRawStream {
            header_line: trim_line_end(&header_line).to_owned(),
            body,
        })
        .expect("send captured raw stream to test");
}

async fn write_attach_stream<S>(stream: &mut S, stream_id: &str, body: &[u8])
where
    S: AsyncWrite + Unpin,
{
    let header = serde_json::to_string(&AttachHeader {
        attach: stream_id.to_owned(),
    })
    .expect("serialize attach header");
    stream
        .write_all(header.as_bytes())
        .await
        .expect("write attach header");
    stream
        .write_all(b"\n")
        .await
        .expect("write attach header newline");
    stream.write_all(body).await.expect("write raw body");
    stream.shutdown().await.expect("close raw stream");
}

fn assert_captured(captured: &CapturedRawStream, expected_stream_id: &str, expected_body: &[u8]) {
    let header: AttachHeader =
        serde_json::from_str(&captured.header_line).expect("parse captured attach header");
    assert_eq!(header.attach, expected_stream_id);
    assert_eq!(captured.body, expected_body);
}

fn trim_line_end(line: &str) -> &str {
    line.trim_end_matches('\n').trim_end_matches('\r')
}

fn unique_socket_path() -> PathBuf {
    let id = NEXT_SOCKET_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "pohunek-client-raw-stream-{}-{id}.sock",
        process::id()
    ))
}
