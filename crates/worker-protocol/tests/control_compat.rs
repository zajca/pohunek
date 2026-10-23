use pohunek_worker_protocol::{
    ActiveIdentityOutcome, ControlCodecError, ControlReader, ControlRequest, ControlResponse,
    ControlWriter, ForwardedActiveReport, IdentityRejectReason, ReportedSession,
    ReportedSessionKind, RequestKind, ResponseKind,
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

#[tokio::test]
async fn previous_worker_claim_without_the_forwarding_fields_is_read_and_written_without_them() {
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
            "phase": "running",
            "worker_process": {"pid": 1, "start_identity": 2},
            "child_process": null,
            "dimensions": null,
            "history_start_offset": 0,
            "next_offset": 0,
            "exit": null,
            "launch_identity": null,
            "active_identity": {
                "provider": "claude",
                "process": {"pid": 5, "start_identity": 6},
                "sequence": 7,
                "expires_at": "2026-10-19T00:00:30Z",
                "reference_kind": null,
                "native_reference": null
            }
        }
    });

    writer.write(&response).await.expect("write previous claim");
    let response = reader
        .read::<ControlResponse>()
        .await
        .expect("read previous claim")
        .expect("response");
    let ResponseKind::Inspected { snapshot } = response.kind else {
        panic!("an inspected response");
    };
    assert_eq!(snapshot.active_identity_revision, None);
    let claim = snapshot.active_identity.expect("claim");
    assert_eq!(
        (&claim.agent, &claim.origin, &claim.reported_session),
        (&None, &None, &None)
    );
    let wire = serde_json::to_value(&claim).expect("serialize claim");
    for field in ["agent", "origin", "reported_session"] {
        assert!(wire.get(field).is_none(), "{field} is omitted when absent");
    }
}

#[test]
fn forwarded_identity_requests_and_answers_keep_their_wire_spelling() {
    let report = json!({
        "request_id": "request-2",
        "type": "record_active_identity",
        "scope": {
            "lease_id": "lease-1",
            "session_id": "s-1",
            "worker_id": "w-1",
            "runtime_id": "runtime-1"
        },
        "report": {
            "provider": "claude",
            "agent": "tuned",
            "process": {"pid": 5, "start_identity": 6},
            "sequence": 7,
            "expires_at": "2026-10-19T00:00:30Z",
            "reported_session": {"kind": "path", "value": "/work/session.jsonl"}
        }
    });
    let request: ControlRequest = serde_json::from_value(report.clone()).expect("decode report");
    assert!(matches!(
        request.kind,
        RequestKind::RecordActiveIdentity { .. }
    ));
    assert_eq!(
        serde_json::to_value(&request).expect("encode report"),
        report
    );

    let release = json!({
        "request_id": "request-3",
        "type": "release_active_identity",
        "scope": {
            "lease_id": "lease-1",
            "session_id": "s-1",
            "worker_id": "w-1",
            "runtime_id": "runtime-1"
        },
        "release": {
            "provider": "claude",
            "process": {"pid": 5, "start_identity": 6},
            "sequence": 8
        }
    });
    let request: ControlRequest = serde_json::from_value(release.clone()).expect("decode release");
    assert!(matches!(
        request.kind,
        RequestKind::ReleaseActiveIdentity { .. }
    ));
    assert_eq!(
        serde_json::to_value(&request).expect("encode release"),
        release
    );

    for (answer, expected) in [
        (
            json!({"status": "accepted", "revision": 3}),
            ActiveIdentityOutcome::Accepted { revision: 3 },
        ),
        (
            json!({"status": "rejected", "reason": "release_unmatched"}),
            ActiveIdentityOutcome::Rejected {
                reason: IdentityRejectReason::ReleaseUnmatched,
            },
        ),
        (
            json!({"status": "rejected", "reason": "sequence_stale"}),
            ActiveIdentityOutcome::Rejected {
                reason: IdentityRejectReason::SequenceStale,
            },
        ),
    ] {
        let response = json!({
            "request_id": "request-4",
            "type": "active_identity_recorded",
            "outcome": answer,
        });
        let decoded: ControlResponse =
            serde_json::from_value(response.clone()).expect("decode answer");
        assert!(
            matches!(decoded.kind, ResponseKind::ActiveIdentityRecorded { outcome } if outcome == expected),
            "{response}"
        );
        assert_eq!(
            serde_json::to_value(&decoded).expect("encode answer"),
            response
        );
    }
}

#[test]
fn a_reported_session_never_appears_in_debug_output() {
    let report = ForwardedActiveReport {
        provider: "claude".to_owned(),
        agent: None,
        process: pohunek_worker_protocol::ProcessIdentity {
            pid: 5,
            start_identity: 6,
        },
        sequence: 7,
        expires_at: "2026-10-19T00:00:30Z".to_owned(),
        reported_session: Some(ReportedSession {
            kind: ReportedSessionKind::Path,
            value: "/home/secret/session.jsonl".to_owned(),
            transcript_path: None,
        }),
    };
    let with_transcript = ReportedSession {
        kind: ReportedSessionKind::Id,
        value: "provider-id".to_owned(),
        transcript_path: Some("/home/secret/transcript.jsonl".to_owned()),
    };

    assert!(
        !format!("{with_transcript:?}").contains("secret"),
        "the transcript path is redacted"
    );
    let rendered = format!("{report:?}");

    assert!(!rendered.contains("secret"), "{rendered}");
    assert!(rendered.contains("[REDACTED]"), "{rendered}");
}

#[test]
fn a_reported_session_keeps_its_transcript_path_and_reads_the_shape_without_one() {
    let both = ReportedSession {
        kind: ReportedSessionKind::Id,
        value: "provider-id".to_owned(),
        transcript_path: Some("/work/transcript.jsonl".to_owned()),
    };
    let wire = serde_json::to_value(&both).expect("encode");
    assert_eq!(
        wire,
        json!({"kind": "id", "value": "provider-id", "transcript_path": "/work/transcript.jsonl"})
    );
    assert_eq!(
        serde_json::from_value::<ReportedSession>(wire).expect("decode"),
        both
    );
    assert!(both.is_valid());

    let without =
        serde_json::from_value::<ReportedSession>(json!({"kind": "id", "value": "provider-id"}))
            .expect("the shape without a transcript path decodes");
    assert_eq!(without.transcript_path, None);
    assert_eq!(
        serde_json::to_value(&without).expect("encode"),
        json!({"kind": "id", "value": "provider-id"}),
        "an unset transcript path is not written"
    );

    let on_a_path = ReportedSession {
        kind: ReportedSessionKind::Path,
        ..both
    };
    assert!(
        !on_a_path.is_valid(),
        "a path annotation carries no second path"
    );
}

#[tokio::test]
async fn the_stream_drain_field_defaults_to_false_and_an_unknown_one_is_ignored() {
    let (reader, writer) = tokio::io::duplex(4096);
    let mut writer = ControlWriter::new(writer);
    let mut reader = ControlReader::new(reader);
    let snapshot = |extra: serde_json::Value| {
        let mut snapshot = json!({
            "session_id": "s-1",
            "worker_id": "worker-1",
            "runtime_id": null,
            "phase": "exited",
            "worker_process": {"pid": 1, "start_identity": 2},
            "child_process": null,
            "dimensions": null,
            "history_start_offset": 0,
            "next_offset": 0,
            "exit": null,
            "launch_identity": null,
            "active_identity": null
        });
        snapshot
            .as_object_mut()
            .expect("snapshot object")
            .extend(extra.as_object().expect("extra fields").clone());
        json!({"request_id": "request-1", "type": "inspected", "snapshot": snapshot})
    };

    for (extra, drains) in [
        (json!({}), false),
        (json!({"drains_streams_on_terminal_ack": true}), true),
        (json!({"a_field_of_a_later_release": 1}), false),
    ] {
        writer
            .write(&snapshot(extra))
            .await
            .expect("write snapshot");
        let response = reader
            .read::<ControlResponse>()
            .await
            .expect("read snapshot")
            .expect("response");
        let ResponseKind::Inspected { snapshot } = response.kind else {
            panic!("an inspect answer");
        };
        assert_eq!(snapshot.drains_streams_on_terminal_ack, drains);
    }
}

#[tokio::test]
async fn previous_worker_answers_without_the_prompt_evidence_fields_are_read_and_written_without_them(
) {
    let (reader, writer) = tokio::io::duplex(4096);
    let mut writer = ControlWriter::new(writer);
    let mut reader = ControlReader::new(reader);
    let snapshot = json!({
        "request_id": "request-1",
        "type": "inspected",
        "snapshot": {
            "session_id": "s-1",
            "worker_id": "worker-1",
            "runtime_id": null,
            "phase": "running",
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
    let write = json!({
        "request_id": "request-2",
        "type": "write_completed",
        "acknowledgement": {"write_id": "input-lease-1-1", "bytes_written": 6}
    });
    writer.write(&snapshot).await.expect("write the snapshot");
    writer
        .write(&write)
        .await
        .expect("write the acknowledgement");

    let ResponseKind::Inspected { snapshot } = reader
        .read::<ControlResponse>()
        .await
        .expect("read the snapshot")
        .expect("response")
        .kind
    else {
        panic!("an inspected response");
    };
    assert_eq!(snapshot.prompt_accepted_sequence, None);
    assert!(serde_json::to_value(&snapshot)
        .expect("serialize the snapshot")
        .get("prompt_accepted_sequence")
        .is_none());

    let ResponseKind::WriteCompleted { acknowledgement } = reader
        .read::<ControlResponse>()
        .await
        .expect("read the acknowledgement")
        .expect("response")
        .kind
    else {
        panic!("a write acknowledgement");
    };
    assert_eq!(acknowledgement.submit_mark_ns, None);
    assert!(serde_json::to_value(&acknowledgement)
        .expect("serialize the acknowledgement")
        .get("submit_mark_ns")
        .is_none());

    // A version-seven decoder that knows neither field reads the version-eight
    // spelling: neither struct rejects unknown fields.
    let with_fields =
        json!({"write_id": "input-lease-1-2", "bytes_written": 6, "submit_mark_ns": 12});
    let tolerant: pohunek_worker_protocol::WriteAck =
        serde_json::from_value(with_fields).expect("decode the version-eight acknowledgement");
    assert_eq!(tolerant.submit_mark_ns, Some(12));
}
