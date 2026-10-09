use pohunek_worker_protocol::{
    ControlCodecError, ControlReader, ControlResponse, ControlWriter, ResponseKind,
    CURRENT_VERSION, PREVIOUS_VERSION,
};
use serde_json::json;

#[tokio::test]
async fn negotiated_response_rejects_reversed_range_and_reads_next_message() {
    let (reader, writer) = tokio::io::duplex(4096);
    let mut writer = ControlWriter::new(writer);
    let mut reader = ControlReader::new(reader);
    let mut response = json!({
        "request_id": "request-1",
        "type": "negotiated",
        "selected_version": CURRENT_VERSION.get(),
        "supported_range": {
            "minimum": CURRENT_VERSION.get(),
            "maximum": PREVIOUS_VERSION.get()
        },
        "session_id": "session-1",
        "worker_id": "worker-1",
        "runtime_id": null,
        "worker_process": { "pid": 1, "start_identity": 1 },
        "phase": "uninitialized",
        "capabilities": [],
        "challenge": "challenge-1"
    });

    writer
        .write(&response)
        .await
        .expect("write reversed response");
    response["supported_range"] = json!({
        "minimum": PREVIOUS_VERSION.get(),
        "maximum": CURRENT_VERSION.get()
    });
    writer.write(&response).await.expect("write valid response");

    let error = reader
        .read::<ControlResponse>()
        .await
        .expect_err("reversed worker range must be rejected");
    assert!(matches!(error, ControlCodecError::Json(_)));
    assert!(error.to_string().contains("reversed"));

    let accepted = reader
        .read::<ControlResponse>()
        .await
        .expect("read after malformed response")
        .expect("valid response");
    assert!(matches!(
        accepted.kind,
        ResponseKind::Negotiated {
            supported_range,
            ..
        } if supported_range.minimum() == PREVIOUS_VERSION
            && supported_range.maximum() == CURRENT_VERSION
    ));
}
