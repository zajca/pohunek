//! Forwards validated notification hook requests to the daemon's public socket.
//!
//! A notification hook inside the managed PTY reaches the worker over the
//! private hook socket. After the worker has attested the caller, this module
//! turns the hook's parameters into one public `notification.create` request
//! bound to the worker's own session and delivers it to the stable daemon
//! socket, so the daemon creates the notification exactly as it does for a
//! hook that dials the daemon directly.

// Rust guideline compliant 2026-09-24

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Public protocol method the forwarded request invokes.
const NOTIFICATION_CREATE_METHOD: &str = "notification.create";

/// Prefix of the public request id; the hook sequence makes it unique.
const REQUEST_ID_PREFIX: &str = "worker-hook";

/// Builds the newline-terminated public request for a hook notification.
///
/// `params` must be a JSON object whose serialized size is at most
/// `max_params_bytes`. Any `session_id` the hook supplied is replaced by the
/// worker's own, so a hook cannot raise a notification for another session.
/// Returns `None` when the parameters are not an object or exceed the bound.
pub(crate) fn build_request_line(
    session_id: &str,
    public_protocol_version: u32,
    sequence: u64,
    params: Value,
    max_params_bytes: usize,
) -> Option<Vec<u8>> {
    let Value::Object(mut params) = params else {
        return None;
    };
    params.insert(
        "session_id".to_owned(),
        Value::String(session_id.to_owned()),
    );
    let params = Value::Object(params);
    if serde_json::to_vec(&params).ok()?.len() > max_params_bytes {
        return None;
    }
    let mut request = Map::new();
    request.insert(
        "v".to_owned(),
        json!({
            "minimum": public_protocol_version,
            "maximum": public_protocol_version,
        }),
    );
    request.insert(
        "id".to_owned(),
        Value::String(format!("{REQUEST_ID_PREFIX}:{session_id}:{sequence}")),
    );
    request.insert(
        "method".to_owned(),
        Value::String(NOTIFICATION_CREATE_METHOD.to_owned()),
    );
    request.insert("params".to_owned(), params);
    let mut line = serde_json::to_vec(&Value::Object(request)).ok()?;
    line.push(b'\n');
    Some(line)
}

/// Sends one request line to the daemon and reports whether it answered `ok`.
///
/// Every failure (connect, write, read, timeout, malformed or error reply)
/// yields `false`; the hook then falls back to its own daemon connection.
pub(crate) async fn deliver(daemon_socket: &Path, line: &[u8], timeout: Duration) -> bool {
    tokio::time::timeout(timeout, exchange(daemon_socket, line))
        .await
        .unwrap_or(false)
}

async fn exchange(daemon_socket: &Path, line: &[u8]) -> bool {
    let Ok(mut stream) = UnixStream::connect(daemon_socket).await else {
        return false;
    };
    if stream.write_all(line).await.is_err() {
        return false;
    }
    let mut reply = String::new();
    if BufReader::new(stream).read_line(&mut reply).await.is_err() {
        return false;
    }
    serde_json::from_str::<Value>(&reply).is_ok_and(|value| value.get("ok").is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_line_binds_the_worker_session_and_public_range() {
        let line = build_request_line(
            "s-1",
            4,
            7,
            json!({"session_id": "s-other", "title": "t"}),
            1024,
        )
        .expect("line");
        assert_eq!(line.last(), Some(&b'\n'));
        let value: Value = serde_json::from_slice(&line).expect("json");
        assert_eq!(value["v"], json!({"minimum": 4, "maximum": 4}));
        assert_eq!(value["method"], json!("notification.create"));
        assert_eq!(value["id"], json!("worker-hook:s-1:7"));
        assert_eq!(value["params"]["session_id"], json!("s-1"));
        assert_eq!(value["params"]["title"], json!("t"));
    }

    #[test]
    fn request_line_rejects_non_objects_and_oversized_params() {
        assert!(build_request_line("s-1", 4, 1, json!([1]), 1024).is_none());
        assert!(build_request_line("s-1", 4, 1, json!({"body": "x".repeat(64)}), 32).is_none());
    }
}
