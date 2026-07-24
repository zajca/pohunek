// Rust guideline compliant 2026-06-26

use std::io::Cursor as SyncCursor;
use std::pin::Pin;
use std::task::{Context, Poll};

use pohunek_worker_protocol::{
    CloseReason, ControlCodecError, ControlMessage, ControlReader, ControlWriter, DataFrame,
    FrameError, FrameHeader, FrameKind, RuntimeId, StreamId, Version, WriteId, CURRENT_VERSION,
    MAX_DATA_HEADER_BYTES, MAX_DATA_PAYLOAD_BYTES,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

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
            runtime_id: RuntimeId::new("runtime-1").expect("valid runtime"),
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
        runtime_id: RuntimeId::new("runtime-1").expect("valid runtime"),
        kind: FrameKind::Close {
            reason: CloseReason::Requested,
        },
    })
    .expect("serialize close header");
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
        runtime_id: RuntimeId::new("runtime-1").expect("valid runtime"),
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

#[test]
fn version_zero_is_rejected_before_it_enters_a_frame_header() {
    let error = Version::new(0).expect_err("version zero must fail");
    assert!(error.to_string().contains("nonzero"));
}
