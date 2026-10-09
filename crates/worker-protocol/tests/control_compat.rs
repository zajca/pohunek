use pohunek_worker_protocol::{
    ControlCodecError, ControlReader, ControlRequest, ControlResponse, ControlWriter, RequestKind,
    ResponseKind,
};
use serde_json::json;

#[tokio::test]
async fn previous_worker_snapshot_without_hook_schema_is_read_over_control_codec() {
    let (reader, writer) = tokio::io::duplex(4096);
    let mut writer = ControlWriter::new(writer);
    let mut reader = ControlReader::new(reader);
    let response = json!({
        "request_id": "request-1",
        "type": "inspected",
        "snapshot": {
            "session_id": "s-1",
            "worker_id": "worker-1",
            "runtime_id": null,
            "phase": "uninitialized",
            "worker_process": {"pid": 1, "start_identity": 2},
            "child_process": null,
            "dimensions": null,
            "history_start_offset": 0,
            "next_offset": 0,
            "exit": null,
            "launch_identity": null,
            "active_identity": null
        }
    });

    writer
        .write(&response)
        .await
        .expect("write previous snapshot");
    let response = reader
        .read::<ControlResponse>()
        .await
        .expect("read previous snapshot")
        .expect("response");
    assert!(matches!(
        response.kind,
        ResponseKind::Inspected { snapshot } if snapshot.hook_schema.is_none()
    ));
}

#[tokio::test]
async fn attach_request_rejects_invalid_dimensions_and_accepts_omitted_size() {
    let (reader, writer) = tokio::io::duplex(4096);
    let mut writer = ControlWriter::new(writer);
    let mut reader = ControlReader::new(reader);
    let mut request = json!({
        "request_id": "request-1",
        "type": "open_data_stream",
        "scope": {
            "lease_id": "lease-1",
            "session_id": "session-1",
            "worker_id": "worker-1",
            "runtime_id": "runtime-1"
        },
        "stream_id": "stream-1",
        "mode": "attach",
        "after_offset": null,
        "attach": {"dimensions": {"columns": 0, "rows": 24}}
    });

    writer.write(&request).await.expect("write invalid attach");
    request["attach"] = json!({});
    writer
        .write(&request)
        .await
        .expect("write attach without size");

    let error = reader
        .read::<ControlRequest>()
        .await
        .expect_err("zero width must be rejected");
    assert!(matches!(error, ControlCodecError::Json(_)));
    assert!(error.to_string().contains("must be nonzero"));

    let accepted = reader
        .read::<ControlRequest>()
        .await
        .expect("read after invalid attach")
        .expect("request");
    assert!(matches!(
        accepted.kind,
        RequestKind::OpenDataStream {
            attach: Some(start),
            ..
        } if start.dimensions.is_none()
    ));
}
