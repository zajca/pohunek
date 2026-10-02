//! The client-facing and logged shape of a durable authentication failure.

// Rust guideline compliant 2026-10-02

use std::sync::{Arc, Mutex};

use axum::{
    body::to_bytes,
    http::StatusCode,
    response::{IntoResponse as _, Response},
};

use super::ApiFailure;
use crate::auth::AuthError;

/// Largest response body these tests read.
const BODY_LIMIT: usize = 4096;

/// Text of a database error that must never reach a client or a log line.
const SECRET_DETAIL: &str = "invalid input syntax for type uuid: \"s3cr3t-token-value\"";

#[derive(Clone)]
pub(super) struct Capture(pub(super) Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("capture lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Returns the response body with its per-request identifier removed.
async fn body_text(response: Response) -> String {
    let bytes = to_bytes(response.into_body(), BODY_LIMIT)
        .await
        .expect("read response body");
    let mut body: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON body");
    body["error"]
        .as_object_mut()
        .expect("error object")
        .remove("request_id")
        .expect("request id");
    body.to_string()
}

#[tokio::test]
async fn a_durable_failure_response_does_not_depend_on_its_cause() {
    let causeless = ApiFailure::auth(&AuthError::Durable(None)).into_response();
    let caused = ApiFailure::auth(&AuthError::durable(&sqlx::Error::Protocol(
        SECRET_DETAIL.to_owned(),
    )))
    .into_response();

    assert_eq!(causeless.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(caused.status(), causeless.status());
    let expected = body_text(causeless).await;
    let actual = body_text(caused).await;
    assert_eq!(actual, expected);
    assert!(actual.contains("unavailable"), "{actual}");
    assert!(!actual.contains("s3cr3t"), "{actual}");
    assert!(!actual.contains("protocol"), "{actual}");
}

#[test]
fn the_boundary_logs_the_sanitized_cause_chain_as_one_structured_event() {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&bytes);
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_writer(move || Capture(Arc::clone(&captured)))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let error = AuthError::durable(&crate::admission::AuthorityError::Cancelled);
        let _ = ApiFailure::auth(&error);
        let _ = ApiFailure::auth(&AuthError::durable(&crate::store::StoreError::Database(
            sqlx::Error::Protocol(SECRET_DETAIL.to_owned()),
        )));
        // A non-durable failure logs nothing.
        let _ = ApiFailure::auth(&AuthError::CredentialInvalid);
    });
    let captured = bytes.lock().expect("capture lock");
    let output = std::str::from_utf8(&captured).expect("UTF-8 JSON");
    assert!(!output.contains("s3cr3t"), "{output}");
    let rows: Vec<serde_json::Value> = output
        .lines()
        .map(|line| serde_json::from_str(line).expect("structured log"))
        .collect();
    assert_eq!(rows.len(), 2, "{output}");
    for row in &rows {
        assert_eq!(row["level"], "ERROR");
        assert_eq!(
            row["fields"]["message"],
            "durable authentication state is unavailable"
        );
    }
    assert_eq!(rows[0]["fields"]["cause"], "relay access was cancelled");
    assert_eq!(
        rows[1]["fields"]["cause"],
        "relay database operation failed: database protocol error"
    );
}
