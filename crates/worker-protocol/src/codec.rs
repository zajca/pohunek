//! Bounded NDJSON control codec.
//!
//! The reader reuses one allocation and scans buffered input before extending
//! it, so an oversized line is rejected before its claimed size is allocated.
//! The writer serializes one JSON value followed by exactly one newline.

// Rust guideline compliant 2026-06-26

use serde::de::DeserializeOwned;
use serde::Serialize;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

/// Maximum JSON bytes in one control line, excluding the newline.
///
/// One MiB accommodates initialization input plans while placing a strict bound
/// on allocation for malformed owner-local peers. This matches pohunek's public
/// control-line limit but remains an independent private-protocol constant.
pub const MAX_CONTROL_LINE_BYTES: usize = 1024 * 1024;

/// Initial reusable control buffer allocation.
///
/// Eight KiB covers routine control messages without allocating the full
/// one-MiB safety bound for every connection.
const INITIAL_CONTROL_BUFFER_BYTES: usize = 8 * 1024;

/// Reads bounded newline-delimited JSON values.
#[derive(Debug)]
pub struct ControlReader<R> {
    inner: BufReader<R>,
    buffer: Vec<u8>,
    maximum: usize,
}

impl<R> ControlReader<R>
where
    R: AsyncRead + Unpin + Send,
{
    /// Creates a reader using [`MAX_CONTROL_LINE_BYTES`].
    #[must_use]
    pub fn new(inner: R) -> Self {
        Self {
            inner: BufReader::new(inner),
            buffer: Vec::new(),
            maximum: MAX_CONTROL_LINE_BYTES,
        }
    }

    /// Creates a reader with an explicit positive line limit.
    ///
    /// This constructor supports deterministic boundary testing and deployments
    /// that choose a stricter limit than the protocol maximum.
    ///
    /// # Errors
    ///
    /// Returns [`ControlCodecError::InvalidLimit`] when `maximum` is zero or
    /// exceeds [`MAX_CONTROL_LINE_BYTES`].
    pub fn with_maximum(inner: R, maximum: usize) -> Result<Self, ControlCodecError> {
        validate_limit(maximum)?;
        Ok(Self {
            inner: BufReader::new(inner),
            buffer: Vec::with_capacity(maximum.min(INITIAL_CONTROL_BUFFER_BYTES)),
            maximum,
        })
    }

    /// Reads and decodes one control message.
    ///
    /// A clean EOF with no buffered bytes returns `Ok(None)`. A final JSON value
    /// without a newline is accepted.
    ///
    /// # Errors
    ///
    /// Returns [`ControlCodecError`] for I/O failures, malformed JSON, or a line
    /// exceeding the configured bound.
    pub async fn read<T>(&mut self) -> Result<Option<T>, ControlCodecError>
    where
        T: DeserializeOwned,
    {
        self.buffer.clear();

        loop {
            let available = self.inner.fill_buf().await?;
            if available.is_empty() {
                return if self.buffer.is_empty() {
                    Ok(None)
                } else {
                    let decoded = decode_line(&self.buffer);
                    scrub(&mut self.buffer);
                    decoded.map(Some)
                };
            }

            if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
                let line_length = self.buffer.len().saturating_add(newline);
                if line_length > self.maximum {
                    let error = ControlCodecError::LineTooLong {
                        actual: line_length,
                        maximum: self.maximum,
                    };
                    scrub(&mut self.buffer);
                    return Err(error);
                }
                self.buffer.extend_from_slice(&available[..newline]);
                self.inner.consume(newline + 1);
                if self.buffer.last() == Some(&b'\r') {
                    self.buffer.pop();
                }
                let decoded = decode_line(&self.buffer);
                scrub(&mut self.buffer);
                return decoded.map(Some);
            }

            let line_length = self.buffer.len().saturating_add(available.len());
            if line_length > self.maximum {
                let error = ControlCodecError::LineTooLong {
                    actual: line_length,
                    maximum: self.maximum,
                };
                scrub(&mut self.buffer);
                return Err(error);
            }
            let consumed = available.len();
            self.buffer.extend_from_slice(available);
            self.inner.consume(consumed);
        }
    }

    /// Consumes the codec and returns its underlying reader.
    #[must_use]
    pub fn into_inner(self) -> R {
        self.inner.into_inner()
    }
}

/// Writes bounded newline-delimited JSON values.
#[derive(Debug)]
pub struct ControlWriter<W> {
    inner: W,
    buffer: Vec<u8>,
    maximum: usize,
}

impl<W> ControlWriter<W>
where
    W: AsyncWrite + Unpin + Send,
{
    /// Creates a writer using [`MAX_CONTROL_LINE_BYTES`].
    #[must_use]
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
            maximum: MAX_CONTROL_LINE_BYTES,
        }
    }

    /// Creates a writer with an explicit positive line limit.
    ///
    /// # Errors
    ///
    /// Returns [`ControlCodecError::InvalidLimit`] when `maximum` is zero or
    /// exceeds [`MAX_CONTROL_LINE_BYTES`].
    pub fn with_maximum(inner: W, maximum: usize) -> Result<Self, ControlCodecError> {
        validate_limit(maximum)?;
        Ok(Self {
            inner,
            buffer: Vec::with_capacity(maximum.min(INITIAL_CONTROL_BUFFER_BYTES)),
            maximum,
        })
    }

    /// Serializes and writes one newline-delimited control value.
    ///
    /// # Errors
    ///
    /// Returns [`ControlCodecError`] for serialization failures, oversized
    /// encoded values, or I/O failures. Partial writes are retried.
    pub async fn write<T>(&mut self, value: &T) -> Result<(), ControlCodecError>
    where
        T: Serialize + Sync,
    {
        self.buffer.clear();
        if let Err(error) = serde_json::to_writer(&mut self.buffer, value) {
            scrub(&mut self.buffer);
            return Err(ControlCodecError::Json(error));
        }
        if self.buffer.len() > self.maximum {
            let error = ControlCodecError::LineTooLong {
                actual: self.buffer.len(),
                maximum: self.maximum,
            };
            scrub(&mut self.buffer);
            return Err(error);
        }
        self.buffer.push(b'\n');
        let result = self.inner.write_all(&self.buffer).await;
        scrub(&mut self.buffer);
        result.map_err(ControlCodecError::Io)
    }

    /// Flushes the underlying stream.
    ///
    /// # Errors
    ///
    /// Returns [`ControlCodecError::Io`] when the stream cannot flush.
    pub async fn flush(&mut self) -> Result<(), ControlCodecError> {
        self.inner.flush().await?;
        Ok(())
    }

    /// Consumes the codec and returns its underlying writer.
    #[must_use]
    pub fn into_inner(self) -> W {
        self.inner
    }
}

fn validate_limit(maximum: usize) -> Result<(), ControlCodecError> {
    if maximum == 0 || maximum > MAX_CONTROL_LINE_BYTES {
        Err(ControlCodecError::InvalidLimit {
            requested: maximum,
            protocol_maximum: MAX_CONTROL_LINE_BYTES,
        })
    } else {
        Ok(())
    }
}

fn decode_line<T>(bytes: &[u8]) -> Result<T, ControlCodecError>
where
    T: DeserializeOwned,
{
    serde_json::from_slice(bytes).map_err(ControlCodecError::Json)
}

fn scrub(bytes: &mut Vec<u8>) {
    bytes.fill(0);
    bytes.clear();
}

/// Reports bounded control-codec failures.
#[derive(Debug, Error)]
pub enum ControlCodecError {
    /// The underlying stream failed.
    #[error("worker control I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// JSON encoding or decoding failed.
    #[error("worker control JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    /// A control line exceeded its configured bound.
    #[error("worker control line is {actual} bytes; maximum is {maximum}")]
    LineTooLong {
        /// Observed or encoded line length.
        actual: usize,
        /// Configured maximum line length.
        maximum: usize,
    },
    /// A caller requested an invalid configured bound.
    #[error("worker control limit {requested} is invalid; valid range is 1..={protocol_maximum}")]
    InvalidLimit {
        /// Requested configured maximum.
        requested: usize,
        /// Hard protocol maximum.
        protocol_maximum: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct Message {
        value: String,
    }

    #[tokio::test]
    async fn reader_accepts_fragmented_crlf_and_final_unterminated_line() {
        let (mut sender, receiver) = tokio::io::duplex(64);
        let task = tokio::spawn(async move {
            for chunk in [
                b"{\"value\":".as_slice(),
                b"\"one\"}\r\n{\"value\":\"two\"}",
            ] {
                sender.write_all(chunk).await.expect("write test chunk");
            }
        });
        let mut reader = ControlReader::with_maximum(receiver, 64).expect("valid limit");

        assert_eq!(
            reader.read::<Message>().await.expect("first message"),
            Some(Message {
                value: "one".to_owned()
            })
        );
        assert_eq!(
            reader.read::<Message>().await.expect("second message"),
            Some(Message {
                value: "two".to_owned()
            })
        );
        assert_eq!(reader.read::<Message>().await.expect("clean EOF"), None);
        task.await.expect("sender task");
    }

    #[tokio::test]
    async fn writer_rejects_oversized_serialization() {
        let mut writer = ControlWriter::with_maximum(tokio::io::sink(), 16).expect("valid limit");
        let error = writer
            .write(&Message {
                value: "too-large-for-limit".to_owned(),
            })
            .await
            .expect_err("oversized line must fail");

        assert!(matches!(error, ControlCodecError::LineTooLong { .. }));
    }
}
