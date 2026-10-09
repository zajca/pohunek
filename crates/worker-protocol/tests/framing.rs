// Rust guideline compliant 2026-06-26

use std::io::Cursor as SyncCursor;
use std::pin::Pin;
use std::task::{Context, Poll};

use pohunek_worker_protocol::{
    Capability, CloseReason, ControlCode, ControlCodecError, ControlError, ControlEvent,
    ControlMessage, ControlReader, ControlRequest, ControlResponse, ControlWriter, DaemonId,
    DataFrame, EventKind, FrameError, FrameHeader, FrameKind, LeaseChallenge, LeaseId,
    ProcessIdentity, RequestId, RequestKind, ResponseKind, RuntimePhase, SessionId, StreamId,
    VersionRange, WorkerId, WorkerInstanceId, WriteId, CURRENT_VERSION, MAX_DATA_HEADER_BYTES,
    MAX_DATA_PAYLOAD_BYTES, PREVIOUS_VERSION,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};

#[derive(Debug)]
struct ChunkReader {
    bytes: SyncCursor<Vec<u8>>,
    maximum_chunk: usize,
}

impl ChunkReader {
    fn new(bytes: Vec<u8>, maximum_chunk: usize) -> Self {
        Self {
            bytes: SyncCursor::new(bytes),
            maximum_chunk,
        }
    }
}

impl AsyncRead for ChunkReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let position = usize::try_from(self.bytes.position()).expect("test position fits usize");
        let source = self.bytes.get_ref();
        let count = source
            .len()
            .saturating_sub(position)
            .min(buf.remaining())
            .min(self.maximum_chunk);
        if count > 0 {
            buf.put_slice(&source[position..position + count]);
            self.bytes
                .set_position(u64::try_from(position + count).expect("test position fits u64"));
        }
        Poll::Ready(Ok(()))
    }
}

#[derive(Debug)]
struct ChunkWriter {
    bytes: Vec<u8>,
    maximum_chunk: usize,
}

impl ChunkWriter {
    fn new(maximum_chunk: usize) -> Self {
        Self {
            bytes: Vec::new(),
            maximum_chunk,
        }
    }
}

impl AsyncWrite for ChunkWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let count = buf.len().min(self.maximum_chunk);
        self.bytes.extend_from_slice(&buf[..count]);
        Poll::Ready(Ok(count))
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn output_frame(payload: Vec<u8>) -> DataFrame {
    DataFrame::new(
        FrameHeader {
            version: CURRENT_VERSION,
            stream_id: StreamId::new("stream-1").expect("valid stream"),
            worker_instance_id: WorkerInstanceId::new("runtime-1").expect("valid runtime"),
            kind: FrameKind::Output { offset: 42 },
        },
        payload,
    )
    .expect("valid output frame")
}

fn raw_frame(header: &[u8], payload: &[u8]) -> Vec<u8> {
    let header_length = u32::try_from(header.len()).expect("test header fits u32");
    let payload_length = u32::try_from(payload.len()).expect("test payload fits u32");
    let mut bytes = Vec::with_capacity(8 + header.len() + payload.len());
    bytes.extend_from_slice(&header_length.to_be_bytes());
    bytes.extend_from_slice(header);
    bytes.extend_from_slice(&payload_length.to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

#[tokio::test]
async fn data_frame_round_trip_survives_single_byte_reads_and_writes() {
    let expected = output_frame(vec![0, 255, 10, 0, 128]);
    let mut writer = ChunkWriter::new(1);
    pohunek_worker_protocol::write_frame(&mut writer, &expected)
        .await
        .expect("write frame through partial writer");

    let mut reader = ChunkReader::new(writer.bytes, 1);
    let decoded = pohunek_worker_protocol::read_frame(&mut reader)
        .await
        .expect("read frame through partial reader")
        .expect("one frame");

    assert_eq!(decoded, expected);
    assert_eq!(
        pohunek_worker_protocol::read_frame(&mut reader)
            .await
            .expect("clean EOF"),
        None
    );
}

#[tokio::test]
async fn every_data_framing_boundary_rejects_truncation() {
    let frame = output_frame(vec![1, 2, 3, 4]);
    let mut writer = ChunkWriter::new(usize::MAX);
    pohunek_worker_protocol::write_frame(&mut writer, &frame)
        .await
        .expect("encode frame");

    let header_length =
        u32::from_be_bytes(writer.bytes[0..4].try_into().expect("length prefix")) as usize;
    let payload_prefix = 4 + header_length;
    let boundaries = [
        1,
        3,
        4,
        4 + header_length - 1,
        4 + header_length,
        payload_prefix + 1,
        payload_prefix + 3,
        payload_prefix + 4,
        writer.bytes.len() - 1,
    ];

    for boundary in boundaries {
        let mut reader = ChunkReader::new(writer.bytes[..boundary].to_vec(), 2);
        let error = pohunek_worker_protocol::read_frame(&mut reader)
            .await
            .expect_err("truncated frame must fail");
        assert!(
            matches!(error, FrameError::UnexpectedEof),
            "boundary {boundary} returned {error:?}"
        );
    }
}

#[tokio::test]
async fn malformed_unknown_and_mismatched_frames_are_rejected() {
    let malformed = raw_frame(b"{", &[]);
    let mut malformed_reader = ChunkReader::new(malformed, 1);
    assert!(matches!(
        pohunek_worker_protocol::read_frame(&mut malformed_reader).await,
        Err(FrameError::InvalidHeader(_))
    ));

    let unknown_header = br#"{
        "version":2,
        "stream_id":"stream-1",
        "runtime_id":"runtime-1",
        "kind":"future_kind"
    }"#;
    let mut future_kind_reader = ChunkReader::new(raw_frame(unknown_header, &[]), 3);
    assert!(matches!(
        pohunek_worker_protocol::read_frame(&mut future_kind_reader).await,
        Err(FrameError::InvalidHeader(_))
    ));

    let close_header = serde_json::to_vec(&FrameHeader {
        version: CURRENT_VERSION,
        stream_id: StreamId::new("stream-1").expect("valid stream"),
        worker_instance_id: WorkerInstanceId::new("runtime-1").expect("valid runtime"),
        kind: FrameKind::Close {
            reason: CloseReason::Requested,
        },
    })
    .expect("serialize close header");
    let mut zero_version: serde_json::Value =
        serde_json::from_slice(&close_header).expect("decode valid header");
    zero_version["version"] = serde_json::json!(0);
    let zero_version_header = serde_json::to_vec(&zero_version).expect("encode invalid header");
    let mut invalid_frame = ChunkReader::new(raw_frame(&zero_version_header, &[]), 2);
    assert!(matches!(
        pohunek_worker_protocol::read_frame(&mut invalid_frame).await,
        Err(FrameError::InvalidHeader(_))
    ));
    let mut mismatched_reader = ChunkReader::new(raw_frame(&close_header, b"unexpected"), 2);
    assert!(matches!(
        pohunek_worker_protocol::read_frame(&mut mismatched_reader).await,
        Err(FrameError::UnexpectedPayload)
    ));
}

#[tokio::test]
async fn oversized_lengths_are_rejected_before_payload_reads() {
    let oversized_header = u32::try_from(MAX_DATA_HEADER_BYTES + 1).expect("header limit fits u32");
    let mut header_reader = ChunkReader::new(oversized_header.to_be_bytes().to_vec(), usize::MAX);
    assert!(matches!(
        pohunek_worker_protocol::read_frame(&mut header_reader).await,
        Err(FrameError::HeaderTooLarge { .. })
    ));

    let header = serde_json::to_vec(&FrameHeader {
        version: CURRENT_VERSION,
        stream_id: StreamId::new("stream-1").expect("valid stream"),
        worker_instance_id: WorkerInstanceId::new("runtime-1").expect("valid runtime"),
        kind: FrameKind::Input {
            write_id: WriteId::new("write-1").expect("valid write"),
        },
    })
    .expect("serialize input header");
    let mut bytes = raw_frame(&header, &[]);
    let payload_prefix = 4 + header.len();
    bytes[payload_prefix..payload_prefix + 4].copy_from_slice(
        &u32::try_from(MAX_DATA_PAYLOAD_BYTES + 1)
            .expect("payload limit fits u32")
            .to_be_bytes(),
    );
    let mut payload_reader = ChunkReader::new(bytes, usize::MAX);
    assert!(matches!(
        pohunek_worker_protocol::read_frame(&mut payload_reader).await,
        Err(FrameError::PayloadTooLarge { .. })
    ));
}

#[tokio::test]
async fn control_codec_survives_partial_io_and_rejects_unknown_operations() {
    let message = ControlMessage::Request(pohunek_worker_protocol::ControlRequest {
        request_id: pohunek_worker_protocol::RequestId::new("request-1").expect("valid request"),
        kind: pohunek_worker_protocol::RequestKind::Negotiate {
            daemon_instance_id: pohunek_worker_protocol::DaemonId::new("daemon-1")
                .expect("valid daemon"),
            minimum_version: pohunek_worker_protocol::PREVIOUS_VERSION,
            maximum_version: CURRENT_VERSION,
        },
    });
    let mut writer = ControlWriter::new(ChunkWriter::new(1));
    writer
        .write(&message)
        .await
        .expect("write control through partial writer");
    let bytes = writer.into_inner().bytes;
    let mut reader = ControlReader::new(ChunkReader::new(bytes, 1));
    assert_eq!(
        reader
            .read::<ControlMessage>()
            .await
            .expect("read control through partial reader"),
        Some(message)
    );

    let unknown = br#"{"request_id":"request-2","type":"future_operation"}"#.to_vec();
    let mut unknown_reader = ControlReader::new(ChunkReader::new(unknown, 1));
    assert!(matches!(
        unknown_reader.read::<ControlMessage>().await,
        Err(ControlCodecError::Json(_))
    ));
}

#[tokio::test]
async fn control_reader_enforces_the_configured_bound() {
    let mut reader = ControlReader::with_maximum(ChunkReader::new(b"123456789\n".to_vec(), 2), 8)
        .expect("valid limit");
    let error = reader
        .read::<serde_json::Value>()
        .await
        .expect_err("oversized control line must fail");

    assert!(matches!(
        error,
        ControlCodecError::LineTooLong {
            actual: 9,
            maximum: 8
        }
    ));
}

/// Builds the daemon's first handshake request.
fn negotiation_request() -> ControlRequest {
    ControlRequest {
        request_id: RequestId::new("request-1").expect("valid request"),
        kind: RequestKind::Negotiate {
            daemon_instance_id: DaemonId::new("daemon-1").expect("valid daemon"),
            minimum_version: PREVIOUS_VERSION,
            maximum_version: CURRENT_VERSION,
        },
    }
}

/// Builds the worker's handshake acknowledgement.
///
/// The same value is serialized by production code paths and re-decoded, so
/// the expected result never derives from the change under test.
fn negotiated_response() -> ResponseKind {
    ResponseKind::Negotiated {
        selected_version: CURRENT_VERSION,
        supported_range: VersionRange::new(PREVIOUS_VERSION, CURRENT_VERSION)
            .expect("ordered range"),
        session_id: SessionId::new("session-1").expect("valid session"),
        worker_id: WorkerId::new("worker-1").expect("valid worker"),
        worker_instance_id: Some(WorkerInstanceId::new("runtime-1").expect("valid runtime")),
        worker_process: ProcessIdentity {
            pid: 42,
            start_identity: 7,
        },
        phase: RuntimePhase::Uninitialized,
        capabilities: vec![Capability::AtomicReplay],
        challenge: LeaseChallenge::new("connection-bound-challenge").expect("valid challenge"),
    }
}

/// Builds the event with the smallest realistic serialized form, so
/// cancellation and bound scenarios keep every piece inside one duplex buffer.
fn compact_identity_event() -> ControlEvent {
    ControlEvent {
        event_sequence: 1,
        kind: EventKind::IdentityChanged {
            worker_instance_id: WorkerInstanceId::new("runtime-1").expect("valid runtime"),
        },
    }
}

#[tokio::test]
async fn control_writer_and_reader_relay_a_full_handshake_in_both_directions() {
    let negotiation = negotiation_request();
    let negotiated = negotiated_response();
    let acquired = ResponseKind::ControllerAcquired {
        lease_id: LeaseId::new("lease-1").expect("valid lease"),
        capabilities: vec![Capability::DeduplicatedInput],
    };
    let acquisition = ControlRequest {
        request_id: RequestId::new("request-2").expect("valid request"),
        kind: RequestKind::AcquireController {
            daemon_instance_id: DaemonId::new("daemon-1").expect("valid daemon"),
            challenge: LeaseChallenge::new("connection-bound-challenge").expect("valid challenge"),
            requested_capabilities: vec![Capability::DeduplicatedInput],
        },
    };

    // 512 bytes buffer both directions of the handshake, so neither side ever
    // waits on the other's backpressure in this flow.
    let (daemon_side, worker_side) = tokio::io::duplex(512);
    let expected_negotiation = negotiation.clone();
    let expected_negotiated = negotiated.clone();
    let expected_acquisition = acquisition.clone();
    let expected_acquired = acquired.clone();
    let worker = tokio::spawn(async move {
        let (read_half, write_half) = tokio::io::split(worker_side);
        let mut reader = ControlReader::new(read_half);
        let mut writer = ControlWriter::new(write_half);

        let request = reader
            .read::<ControlMessage>()
            .await
            .expect("negotiation read")
            .expect("one negotiation request");
        assert_eq!(
            request,
            ControlMessage::Request(expected_negotiation.clone())
        );
        writer
            .write(&ControlMessage::Response(ControlResponse {
                request_id: RequestId::new("request-1").expect("valid request"),
                kind: expected_negotiated.clone(),
            }))
            .await
            .expect("write negotiated response");

        let request = reader
            .read::<ControlMessage>()
            .await
            .expect("acquisition read")
            .expect("one acquisition request");
        assert_eq!(
            request,
            ControlMessage::Request(expected_acquisition.clone())
        );
        writer
            .write(&ControlMessage::Response(ControlResponse {
                request_id: RequestId::new("request-2").expect("valid request"),
                kind: expected_acquired.clone(),
            }))
            .await
            .expect("write acquisition response");
    });

    let (read_half, write_half) = tokio::io::split(daemon_side);
    let mut writer = ControlWriter::new(write_half);
    let mut reader = ControlReader::new(read_half);
    writer
        .write(&ControlMessage::Request(negotiation.clone()))
        .await
        .expect("write negotiation request");
    let response = reader
        .read::<ControlMessage>()
        .await
        .expect("negotiated read")
        .expect("one negotiated response");
    assert_eq!(
        response,
        ControlMessage::Response(ControlResponse {
            request_id: RequestId::new("request-1").expect("valid request"),
            kind: negotiated.clone(),
        })
    );
    writer
        .write(&ControlMessage::Request(acquisition.clone()))
        .await
        .expect("write acquisition request");
    let response = reader
        .read::<ControlMessage>()
        .await
        .expect("acquired read")
        .expect("one acquired response");
    assert_eq!(
        response,
        ControlMessage::Response(ControlResponse {
            request_id: RequestId::new("request-2").expect("valid request"),
            kind: acquired.clone(),
        })
    );

    worker.await.expect("worker task");
    assert_eq!(
        reader.read::<ControlMessage>().await.expect("clean EOF"),
        None
    );
}

#[tokio::test]
async fn control_reader_accepts_fragmented_crlf_lines_and_a_final_unterminated_line() {
    let negotiated = ControlResponse {
        request_id: RequestId::new("request-1").expect("valid request"),
        kind: negotiated_response(),
    };
    let advanced = ControlEvent {
        event_sequence: 1,
        kind: EventKind::OutputAdvanced {
            worker_instance_id: WorkerInstanceId::new("runtime-1").expect("valid runtime"),
            next_offset: 42,
        },
    };

    // 1024 bytes hold the whole exchange without backpressure; the write-side
    // chunk boundaries below still split the lines explicitly.
    let (codec_side, counterpart_side) = tokio::io::duplex(1024);
    let expected_negotiated = negotiated.clone();
    let expected_advanced = advanced.clone();
    let counterpart = tokio::spawn(async move {
        // Every production peer serializes through the codec, so the exchange
        // opens with its newline-terminated line.
        let mut writer = ControlWriter::new(counterpart_side);
        writer
            .write(&ControlMessage::Request(negotiation_request()))
            .await
            .expect("write negotiate line");
        let mut raw = writer.into_inner();

        let mut crlf_line =
            serde_json::to_vec(&ControlMessage::Response(expected_negotiated.clone()))
                .expect("serialize negotiated response");
        crlf_line.extend_from_slice(b"\r\n");
        let crlf_beginning = crlf_line.len() - 1;
        // The CR lands in the first chunk and its LF opens the next one, so
        // the reader must handle a CRLF pair split across read boundaries.
        raw.write_all(&crlf_line[..crlf_beginning])
            .await
            .expect("write response body with CR");

        let mut unterminated_line = vec![b'\n'];
        unterminated_line.extend_from_slice(
            &serde_json::to_vec(&ControlMessage::Event(expected_advanced.clone()))
                .expect("serialize advanced event"),
        );
        raw.write_all(&unterminated_line)
            .await
            .expect("write newline and final unterminated line");
    });

    let mut reader = ControlReader::new(codec_side);
    assert_eq!(
        reader
            .read::<ControlMessage>()
            .await
            .expect("request decode"),
        Some(ControlMessage::Request(negotiation_request()))
    );
    assert_eq!(
        reader
            .read::<ControlMessage>()
            .await
            .expect("response decode"),
        Some(ControlMessage::Response(negotiated))
    );
    assert_eq!(
        reader.read::<ControlMessage>().await.expect("event decode"),
        Some(ControlMessage::Event(advanced))
    );
    assert_eq!(
        reader.read::<ControlMessage>().await.expect("clean EOF"),
        None
    );
    counterpart.await.expect("counterpart task");
}

/// Polls one `read` exactly once, returning `None` when it was still pending
/// and its future was dropped.
async fn poll_read_once<R>(
    reader: &mut ControlReader<R>,
) -> Option<Result<Option<ControlMessage>, ControlCodecError>>
where
    R: AsyncRead + Unpin + Send,
{
    tokio::select! {
        biased;
        result = reader.read::<ControlMessage>() => Some(result),
        () = std::future::ready(()) => None,
    }
}

#[tokio::test]
async fn a_dropped_pending_call_resumes_the_interrupted_line() {
    // The 64-byte duplex buffer holds every piece because the polls between
    // the writes drain it, so no write ever waits on a concurrent reader.
    let (mut sender, receiver) = tokio::io::duplex(64);
    let expected = compact_identity_event();
    let mut line = serde_json::to_vec(&ControlMessage::Event(expected.clone()))
        .expect("serialize identity event");
    line.push(b'\n');
    let mut reader = ControlReader::new(receiver);

    // The 8-byte prefix keeps a JSON object unterminated, so the two dropped
    // calls below each poll an interrupted line and lose only their future.
    sender.write_all(&line[..8]).await.expect("write prefix");
    assert!(
        poll_read_once(&mut reader).await.is_none(),
        "an unterminated line must leave the read pending"
    );
    sender.write_all(&line[8..56]).await.expect("write body");
    assert!(
        poll_read_once(&mut reader).await.is_none(),
        "a longer unterminated line must still leave the read pending"
    );
    sender
        .write_all(&line[56..])
        .await
        .expect("write terminator");

    // The decoded full line is the only external evidence that the prefix
    // survived the dropped calls: codec scrubbing stays an internal detail.
    assert_eq!(
        reader.read::<ControlMessage>().await.expect("resumed line"),
        Some(ControlMessage::Event(expected))
    );
}

#[tokio::test]
async fn a_resumed_prefix_stays_bound_by_the_configured_limit() {
    // Each leftover piece plus the prefix fits in the 64-byte duplex buffer,
    // while the reader limit sits just below the rendered line so the
    // accumulated prefix is what trips the bound.
    let (mut sender, receiver) = tokio::io::duplex(64);
    let expected = compact_identity_event();
    let mut line = serde_json::to_vec(&ControlMessage::Event(expected.clone()))
        .expect("serialize identity event");
    line.push(b'\n');
    let mut reader = ControlReader::with_maximum(receiver, line.len() - 3).expect("valid limit");

    sender.write_all(&line[..8]).await.expect("write prefix");
    assert!(
        poll_read_once(&mut reader).await.is_none(),
        "an unterminated line must leave the read pending"
    );
    // Everything between the prefix and the newline overflows the limit
    // without the line ever terminating.
    sender
        .write_all(&line[8..line.len() - 1])
        .await
        .expect("write body");
    let result = poll_read_once(&mut reader)
        .await
        .expect("an overlong resumed line must fail without waiting");

    match result {
        Err(ControlCodecError::LineTooLong { actual, maximum }) => {
            assert_eq!(actual, line.len() - 1);
            assert_eq!(maximum, line.len() - 3);
        }
        other => panic!("expected a LineTooLong error, got {other:?}"),
    }
}

#[tokio::test]
async fn an_oversized_serialized_line_is_rejected_with_a_clean_stream() {
    // 128 bytes of duplex and a 96-byte writer bound let the realistic identity
    // event through, while any realistic runtime fault would overflow it.
    let (codec_side, write_side) = tokio::io::duplex(128);
    let mut writer = ControlWriter::with_maximum(write_side, 96).expect("valid limit");
    let fault = ControlMessage::Event(ControlEvent {
        event_sequence: 2,
        kind: EventKind::RuntimeFault {
            worker_instance_id: None,
            error: ControlError {
                code: ControlCode::RuntimeFault,
                message: "sanitized failure repeated to overflow the bound certainly ".repeat(8),
                retryable: false,
            },
        },
    });

    let error = writer
        .write(&fault)
        .await
        .expect_err("oversized line must fail");
    assert!(matches!(error, ControlCodecError::LineTooLong { .. }));

    // Whatever the failed attempt kept had to leave the stream untouched: the
    // compact event that follows decodes standalone and no byte is missing.
    let compact = compact_identity_event();
    writer
        .write(&ControlMessage::Event(compact.clone()))
        .await
        .expect("write compact event after the rejected one");

    drop(writer);
    let mut reader = ControlReader::new(codec_side);
    assert_eq!(
        reader
            .read::<ControlMessage>()
            .await
            .expect("compact event"),
        Some(ControlMessage::Event(compact))
    );
    assert_eq!(
        reader.read::<ControlMessage>().await.expect("clean EOF"),
        None
    );
}
