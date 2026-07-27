//! Binary-safe worker data framing.
//!
//! Each frame contains a four-byte big-endian JSON-header length, the JSON
//! header, a four-byte big-endian payload length, and opaque payload bytes.
//! Lengths are checked before allocation. Async helpers tolerate arbitrary
//! partial reads and writes.

// Rust guideline compliant 2026-07-27

use std::fmt::{Debug, Formatter};
use std::io::ErrorKind;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{
    ControlError, DataToken, Dimensions, ExitStatus, RuntimeId, StreamId, StreamMode, Version,
    WriteId,
};

/// Maximum serialized JSON header bytes in one data frame.
///
/// Terminal snapshots include structured visible lines in the header. The
/// 256-KiB limit accommodates large terminals while bounding allocations before
/// parsing untrusted local input.
pub const MAX_DATA_HEADER_BYTES: usize = 256 * 1024;

/// Maximum opaque payload bytes in one data frame.
///
/// One MiB is much larger than normal PTY read chunks while preventing an
/// owner-local peer from forcing an unbounded allocation.
pub const MAX_DATA_PAYLOAD_BYTES: usize = 1024 * 1024;

/// Wire length prefixes are unsigned 32-bit big-endian integers.
const LENGTH_PREFIX_BYTES: usize = size_of::<u32>();

/// Describes the terminal cursor position and visibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    /// Zero-based terminal column.
    pub column: u16,
    /// Zero-based terminal row.
    pub row: u16,
    /// Whether the cursor is visible.
    pub visible: bool,
}

/// Captures complete provider-independent terminal state.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSnapshot {
    /// Output offset represented by this snapshot.
    pub watermark: u64,
    /// Terminal dimensions.
    pub dimensions: Dimensions,
    /// Current cursor state.
    pub cursor: Cursor,
    /// Whether the alternate screen is active.
    pub alternate_screen: bool,
    /// Current terminal title.
    pub title: Option<String>,
    /// Current sanitized progress signal.
    pub progress: Option<String>,
    /// Current visible terminal rows without ANSI control bytes.
    pub visible_lines: Vec<String>,
}

impl Debug for TerminalSnapshot {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalSnapshot")
            .field("watermark", &self.watermark)
            .field("dimensions", &self.dimensions)
            .field("cursor", &self.cursor)
            .field("alternate_screen", &self.alternate_screen)
            .field("title", &self.title.as_ref().map(|_| "<redacted>"))
            .field("progress", &self.progress.as_ref().map(|_| "<redacted>"))
            .field("visible_line_count", &self.visible_lines.len())
            .field("visible_lines", &"<redacted>")
            .finish()
    }
}

/// Explains why a framed data stream closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    /// The requester closed the stream normally.
    Requested,
    /// The runtime reached a terminal outcome.
    RuntimeExited,
    /// The subscriber remained too slow after snapshot recovery.
    SubscriberTooSlow,
    /// The controller lease ended.
    LeaseReleased,
}

/// Describes one data frame and its kind-specific metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FrameKind {
    /// Redeems a token and opens a framed stream.
    Open {
        /// One-use stream credential.
        token: DataToken,
        /// Stream purpose.
        mode: StreamMode,
        /// Last processed offset on reconnection.
        after_offset: Option<u64>,
    },
    /// Replays retained output beginning at `offset`.
    Replay {
        /// Offset of the first payload byte.
        offset: u64,
    },
    /// Carries newly observed PTY output.
    Output {
        /// Offset of the first payload byte.
        offset: u64,
    },
    /// Carries structured state and an ANSI repaint payload.
    ///
    /// A repaint may span multiple frames with identical snapshot metadata.
    /// Receivers preserve the payload order of frames for the same snapshot.
    TerminalSnapshot {
        /// Complete terminal state.
        snapshot: TerminalSnapshot,
    },
    /// Reports an output range no longer retained.
    Gap {
        /// First missing byte offset.
        missing_start: u64,
        /// Offset immediately after the missing range.
        missing_end: u64,
        /// Snapshot and live-output resume watermark.
        watermark: u64,
    },
    /// Carries opaque raw attach input.
    Input {
        /// Runtime-unique idempotency key.
        write_id: WriteId,
    },
    /// Acknowledges one raw attach input chunk.
    InputAck {
        /// Completed raw input operation.
        write_id: WriteId,
        /// Bytes written and flushed.
        bytes_written: u64,
    },
    /// Reports the child terminal outcome.
    Exit {
        /// Recorded process outcome.
        exit: ExitStatus,
    },
    /// Reports a typed stream-local failure.
    Error {
        /// Structured sanitized failure.
        error: ControlError,
    },
    /// Closes the stream without terminating the runtime.
    Close {
        /// Typed close reason.
        reason: CloseReason,
    },
}

/// Carries common metadata for one data frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameHeader {
    /// Negotiated worker protocol version.
    pub version: Version,
    /// Framed data stream identity.
    pub stream_id: StreamId,
    /// Uninterrupted PTY runtime generation.
    pub runtime_id: RuntimeId,
    /// Kind-specific metadata.
    #[serde(flatten)]
    pub kind: FrameKind,
}

/// Carries one validated frame with an opaque payload.
#[derive(Clone, PartialEq, Eq)]
pub struct DataFrame {
    header: FrameHeader,
    payload: Vec<u8>,
}

impl DataFrame {
    /// Creates and validates one frame.
    ///
    /// # Errors
    ///
    /// Returns [`FrameError`] when payload presence contradicts the frame kind,
    /// a length exceeds protocol bounds, or an output range overflows.
    pub fn new(header: FrameHeader, mut payload: Vec<u8>) -> Result<Self, FrameError> {
        if let Err(error) = validate_payload(&header.kind, &payload) {
            payload.fill(0);
            return Err(error);
        }
        if payload.len() > MAX_DATA_PAYLOAD_BYTES {
            let error = FrameError::PayloadTooLarge {
                actual: payload.len(),
                maximum: MAX_DATA_PAYLOAD_BYTES,
            };
            payload.fill(0);
            return Err(error);
        }
        Ok(Self { header, payload })
    }

    /// Borrows frame metadata.
    #[must_use]
    pub fn header(&self) -> &FrameHeader {
        &self.header
    }

    /// Borrows opaque payload bytes.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Consumes the frame into metadata and payload.
    #[must_use]
    pub fn into_parts(self) -> (FrameHeader, Vec<u8>) {
        (self.header, self.payload)
    }
}

impl Debug for DataFrame {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataFrame")
            .field("header", &self.header)
            .field("payload", &"<redacted>")
            .field("payload_bytes", &self.payload.len())
            .finish()
    }
}

fn validate_payload(kind: &FrameKind, payload: &[u8]) -> Result<(), FrameError> {
    match kind {
        FrameKind::Replay { offset } | FrameKind::Output { offset } => {
            if payload.is_empty() {
                return Err(FrameError::PayloadRequired);
            }
            let payload_len = u64::try_from(payload.len()).map_err(|_conversion| {
                FrameError::PayloadTooLarge {
                    actual: payload.len(),
                    maximum: MAX_DATA_PAYLOAD_BYTES,
                }
            })?;
            offset
                .checked_add(payload_len)
                .ok_or(FrameError::OffsetOverflow)?;
        }
        FrameKind::TerminalSnapshot { .. } | FrameKind::Input { .. } => {
            if payload.is_empty() {
                return Err(FrameError::PayloadRequired);
            }
        }
        FrameKind::Gap {
            missing_start,
            missing_end,
            watermark,
        } => {
            if !payload.is_empty() {
                return Err(FrameError::UnexpectedPayload);
            }
            if missing_start >= missing_end || missing_end > watermark {
                return Err(FrameError::InvalidGap {
                    missing_start: *missing_start,
                    missing_end: *missing_end,
                    watermark: *watermark,
                });
            }
        }
        FrameKind::Open { .. }
        | FrameKind::InputAck { .. }
        | FrameKind::Exit { .. }
        | FrameKind::Error { .. }
        | FrameKind::Close { .. } => {
            if !payload.is_empty() {
                return Err(FrameError::UnexpectedPayload);
            }
        }
    }
    Ok(())
}

/// Reports data-frame encoding, decoding, or validation failures.
#[derive(Debug, Error)]
pub enum FrameError {
    /// The underlying stream failed.
    #[error("worker data-frame I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// The stream ended partway through a frame.
    #[error("worker data stream ended partway through a frame")]
    UnexpectedEof,
    /// A serialized header exceeded its allocation bound.
    #[error("worker data header is {actual} bytes; maximum is {maximum}")]
    HeaderTooLarge {
        /// Observed serialized length.
        actual: usize,
        /// Maximum accepted length.
        maximum: usize,
    },
    /// A payload exceeded its allocation bound.
    #[error("worker data payload is {actual} bytes; maximum is {maximum}")]
    PayloadTooLarge {
        /// Observed payload length.
        actual: usize,
        /// Maximum accepted length.
        maximum: usize,
    },
    /// A JSON header was malformed or had an unknown frame kind.
    #[error("worker data header is invalid: {0}")]
    InvalidHeader(#[from] serde_json::Error),
    /// A byte-carrying frame omitted its payload.
    #[error("worker data frame kind requires a nonempty payload")]
    PayloadRequired,
    /// A metadata-only frame unexpectedly carried bytes.
    #[error("worker data frame kind does not permit a payload")]
    UnexpectedPayload,
    /// An output frame's byte range overflowed.
    #[error("worker data frame output range overflows u64 offsets")]
    OffsetOverflow,
    /// A gap had inconsistent offsets.
    #[error("worker data gap {missing_start}..{missing_end} is invalid for watermark {watermark}")]
    InvalidGap {
        /// First claimed missing offset.
        missing_start: u64,
        /// Offset after the claimed missing range.
        missing_end: u64,
        /// Claimed snapshot watermark.
        watermark: u64,
    },
}

/// Reads one complete data frame.
///
/// A clean EOF before any length-prefix byte returns `Ok(None)`. Once a frame
/// starts, EOF is reported as [`FrameError::UnexpectedEof`].
///
/// # Errors
///
/// Returns [`FrameError`] for I/O failures, incomplete frames, oversized
/// lengths, malformed or unknown headers, and semantic frame mismatches.
pub async fn read_frame<R>(reader: &mut R) -> Result<Option<DataFrame>, FrameError>
where
    R: AsyncRead + Unpin + Send,
{
    let Some(header_length) = read_length(reader).await? else {
        return Ok(None);
    };
    if header_length > MAX_DATA_HEADER_BYTES {
        return Err(FrameError::HeaderTooLarge {
            actual: header_length,
            maximum: MAX_DATA_HEADER_BYTES,
        });
    }

    let mut header_bytes = vec![0; header_length];
    read_exact_frame(reader, &mut header_bytes).await?;
    let header = serde_json::from_slice::<FrameHeader>(&header_bytes);
    header_bytes.fill(0);
    let header = header?;

    let payload_length = read_required_length(reader).await?;
    if payload_length > MAX_DATA_PAYLOAD_BYTES {
        return Err(FrameError::PayloadTooLarge {
            actual: payload_length,
            maximum: MAX_DATA_PAYLOAD_BYTES,
        });
    }
    let mut payload = vec![0; payload_length];
    read_exact_frame(reader, &mut payload).await?;

    DataFrame::new(header, payload).map(Some)
}

/// Writes one complete data frame.
///
/// # Errors
///
/// Returns [`FrameError`] for invalid frames, oversized serialized headers, or
/// I/O failures. Partial writes are retried until the complete frame is sent.
pub async fn write_frame<W>(writer: &mut W, frame: &DataFrame) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin + Send,
{
    validate_payload(&frame.header.kind, &frame.payload)?;
    let mut header = serde_json::to_vec(&frame.header)?;
    if header.len() > MAX_DATA_HEADER_BYTES {
        let error = FrameError::HeaderTooLarge {
            actual: header.len(),
            maximum: MAX_DATA_HEADER_BYTES,
        };
        header.fill(0);
        return Err(error);
    }
    if frame.payload.len() > MAX_DATA_PAYLOAD_BYTES {
        return Err(FrameError::PayloadTooLarge {
            actual: frame.payload.len(),
            maximum: MAX_DATA_PAYLOAD_BYTES,
        });
    }

    let write_result = async {
        write_length(writer, header.len()).await?;
        writer.write_all(&header).await?;
        write_length(writer, frame.payload.len()).await?;
        writer.write_all(&frame.payload).await?;
        Ok::<(), FrameError>(())
    }
    .await;
    header.fill(0);
    write_result?;
    Ok(())
}

async fn read_length<R>(reader: &mut R) -> Result<Option<usize>, FrameError>
where
    R: AsyncRead + Unpin + Send,
{
    let mut bytes = [0_u8; LENGTH_PREFIX_BYTES];
    let mut read = 0;
    while read < bytes.len() {
        let count = reader.read(&mut bytes[read..]).await?;
        if count == 0 {
            return if read == 0 {
                Ok(None)
            } else {
                Err(FrameError::UnexpectedEof)
            };
        }
        read += count;
    }
    Ok(Some(u32::from_be_bytes(bytes) as usize))
}

async fn read_required_length<R>(reader: &mut R) -> Result<usize, FrameError>
where
    R: AsyncRead + Unpin + Send,
{
    read_length(reader).await?.ok_or(FrameError::UnexpectedEof)
}

async fn read_exact_frame<R>(reader: &mut R, bytes: &mut [u8]) -> Result<(), FrameError>
where
    R: AsyncRead + Unpin + Send,
{
    match reader.read_exact(bytes).await {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::UnexpectedEof => Err(FrameError::UnexpectedEof),
        Err(error) => Err(FrameError::Io(error)),
    }
}

async fn write_length<W>(writer: &mut W, length: usize) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin + Send,
{
    let length = u32::try_from(length).map_err(|_conversion| FrameError::PayloadTooLarge {
        actual: length,
        maximum: u32::MAX as usize,
    })?;
    writer.write_all(&length.to_be_bytes()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CURRENT_VERSION;

    fn header(kind: FrameKind) -> FrameHeader {
        FrameHeader {
            version: CURRENT_VERSION,
            stream_id: StreamId::new("stream-1").expect("valid stream"),
            runtime_id: RuntimeId::new("runtime-1").expect("valid runtime"),
            kind,
        }
    }

    #[test]
    fn metadata_frame_rejects_payload() {
        let error = DataFrame::new(
            header(FrameKind::Open {
                token: DataToken::new("token-1").expect("valid token"),
                mode: StreamMode::Attach,
                after_offset: None,
            }),
            vec![1],
        )
        .expect_err("open frame must be metadata-only");

        assert!(matches!(error, FrameError::UnexpectedPayload));
    }

    #[test]
    fn output_range_cannot_overflow() {
        let error = DataFrame::new(header(FrameKind::Output { offset: u64::MAX }), vec![1])
            .expect_err("overflowing output range must fail");

        assert!(matches!(error, FrameError::OffsetOverflow));
    }

    #[test]
    fn frame_debug_redacts_payload_and_snapshot_text() {
        let secret = "terminal_secret_that_must_not_leak";
        let frame = DataFrame::new(
            header(FrameKind::TerminalSnapshot {
                snapshot: TerminalSnapshot {
                    watermark: 10,
                    dimensions: Dimensions::new(80, 24).expect("valid dimensions"),
                    cursor: Cursor {
                        column: 0,
                        row: 0,
                        visible: true,
                    },
                    alternate_screen: true,
                    title: Some(secret.to_owned()),
                    progress: Some(secret.to_owned()),
                    visible_lines: vec![secret.to_owned()],
                },
            }),
            secret.as_bytes().to_vec(),
        )
        .expect("valid snapshot frame");

        let rendered = format!("{frame:?}");
        assert!(!rendered.contains(secret));
        assert!(rendered.contains("<redacted>"));
    }
}
