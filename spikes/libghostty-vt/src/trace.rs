//! The PTY trace file format.
//!
//! ```text
//! file    = magic version rows cols record*
//! magic   = "PHTRACE"                     (7 bytes)
//! version = u16 little endian             (currently 1)
//! rows    = u16 LE, cols = u16 LE         (initial size)
//! record  = 0x01 len:u32-LE bytes[len]    Output: exactly one PTY read chunk
//!         | 0x02 rows:u16-LE cols:u16-LE  Resize
//! ```
//!
//! Chunk boundaries of `Output` records are kept as read, so a replay can feed
//! the model the way the worker fed it. Every length is checked against its
//! bound before any byte is sliced or any buffer is reserved, and a malformed
//! input is a [`TraceError`], never a panic.

// Rust guideline compliant 2026-06-26

use std::fmt;

/// First seven bytes of every trace file.
pub const MAGIC: &[u8; 7] = b"PHTRACE";

/// The format version this code reads and writes.
pub const VERSION: u16 = 1;

/// Record tag of an `Output` record.
const TAG_OUTPUT: u8 = 1;

/// Record tag of a `Resize` record.
const TAG_RESIZE: u8 = 2;

/// Bytes of the file header: magic, version, rows, cols.
const HEADER_BYTES: usize = 7 + 2 + 2 + 2;

/// Bounds the parser and the encoder enforce. Built from the config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Largest accepted file.
    pub max_trace_bytes: usize,
    /// Largest accepted `Output` record.
    pub max_record_bytes: usize,
    /// Most accepted records.
    pub max_records: usize,
    /// Smallest row or column count.
    pub min_dimension: u16,
    /// Largest row count.
    pub max_rows: u16,
    /// Largest column count.
    pub max_cols: u16,
}

impl Limits {
    fn dimensions_ok(&self, rows: u16, cols: u16) -> bool {
        (self.min_dimension..=self.max_rows).contains(&rows)
            && (self.min_dimension..=self.max_cols).contains(&cols)
    }
}

/// One entry of a trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    /// One PTY read chunk.
    Output(Vec<u8>),
    /// The PTY was resized.
    Resize {
        /// New row count.
        rows: u16,
        /// New column count.
        cols: u16,
    },
}

/// A parsed trace: the initial size and the ordered records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace {
    /// Initial row count.
    pub rows: u16,
    /// Initial column count.
    pub cols: u16,
    /// The records in recording order.
    pub records: Vec<Record>,
}

/// What is wrong with a trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceErrorKind {
    /// The file is larger than `max_trace_bytes`.
    TraceTooLarge {
        /// Size of the input.
        len: usize,
        /// The bound.
        max: usize,
    },
    /// The first bytes are not the magic.
    BadMagic,
    /// The version is not [`VERSION`].
    UnsupportedVersion {
        /// The version found.
        found: u16,
    },
    /// The input ends inside a header field or a record.
    Truncated {
        /// The part that is cut off.
        part: &'static str,
    },
    /// A record tag is neither `Output` nor `Resize`.
    UnknownRecordKind {
        /// The tag byte.
        tag: u8,
    },
    /// An `Output` record is longer than `max_record_bytes`.
    RecordTooLarge {
        /// Declared length.
        len: usize,
        /// The bound.
        max: usize,
    },
    /// There are more than `max_records` records.
    TooManyRecords {
        /// The bound.
        max: usize,
    },
    /// A size is outside the configured dimension bounds.
    InvalidDimensions {
        /// Row count found.
        rows: u16,
        /// Column count found.
        cols: u16,
    },
}

impl fmt::Display for TraceErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TraceTooLarge { len, max } => write!(f, "trace is {len} bytes, bound is {max}"),
            Self::BadMagic => f.write_str("not a trace file (bad magic)"),
            Self::UnsupportedVersion { found } => {
                write!(f, "unsupported trace version {found}, expected {VERSION}")
            }
            Self::Truncated { part } => write!(f, "input ends inside {part}"),
            Self::UnknownRecordKind { tag } => write!(f, "unknown record kind {tag}"),
            Self::RecordTooLarge { len, max } => {
                write!(f, "output record of {len} bytes exceeds the bound {max}")
            }
            Self::TooManyRecords { max } => write!(f, "more than {max} records"),
            Self::InvalidDimensions { rows, cols } => {
                write!(
                    f,
                    "dimensions {rows}x{cols} are outside the configured bounds"
                )
            }
        }
    }
}

/// A trace could not be parsed or encoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("trace error at byte {offset}: {kind}")]
pub struct TraceError {
    kind: TraceErrorKind,
    offset: usize,
}

impl TraceError {
    fn new(kind: TraceErrorKind, offset: usize) -> Self {
        Self { kind, offset }
    }

    /// What is wrong.
    #[must_use]
    pub fn kind(&self) -> &TraceErrorKind {
        &self.kind
    }

    /// Byte offset in the input where the problem was found.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.offset
    }
}

/// Cursor over the input that reports a typed error at the failing offset.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize, part: &'static str) -> Result<&'a [u8], TraceError> {
        let end = self
            .at
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| TraceError::new(TraceErrorKind::Truncated { part }, self.at))?;
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    fn u16(&mut self, part: &'static str) -> Result<u16, TraceError> {
        let bytes = self.take(2, part)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self, part: &'static str) -> Result<u32, TraceError> {
        let bytes = self.take(4, part)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn done(&self) -> bool {
        self.at == self.bytes.len()
    }
}

impl Trace {
    /// Parses a trace file.
    ///
    /// # Errors
    ///
    /// Returns a [`TraceError`] for a wrong magic or version, a truncated
    /// header or record, an unknown record kind, an `Output` record over
    /// `limits.max_record_bytes`, more than `limits.max_records` records, or a
    /// size outside the dimension bounds.
    pub fn parse(bytes: &[u8], limits: &Limits) -> Result<Self, TraceError> {
        if bytes.len() > limits.max_trace_bytes {
            return Err(TraceError::new(
                TraceErrorKind::TraceTooLarge {
                    len: bytes.len(),
                    max: limits.max_trace_bytes,
                },
                0,
            ));
        }
        let mut reader = Reader { bytes, at: 0 };
        let magic = reader.take(MAGIC.len(), "the magic")?;
        if magic != MAGIC {
            return Err(TraceError::new(TraceErrorKind::BadMagic, 0));
        }
        let version = reader.u16("the version")?;
        if version != VERSION {
            return Err(TraceError::new(
                TraceErrorKind::UnsupportedVersion { found: version },
                MAGIC.len(),
            ));
        }
        let rows = reader.u16("the initial rows")?;
        let cols = reader.u16("the initial cols")?;
        if !limits.dimensions_ok(rows, cols) {
            return Err(TraceError::new(
                TraceErrorKind::InvalidDimensions { rows, cols },
                MAGIC.len() + 2,
            ));
        }
        let mut records = Vec::new();
        while !reader.done() {
            let start = reader.at;
            if records.len() == limits.max_records {
                return Err(TraceError::new(
                    TraceErrorKind::TooManyRecords {
                        max: limits.max_records,
                    },
                    start,
                ));
            }
            let tag = reader.take(1, "a record tag")?[0];
            records.push(match tag {
                TAG_OUTPUT => {
                    let len = reader.u32("an output length")? as usize;
                    if len > limits.max_record_bytes {
                        return Err(TraceError::new(
                            TraceErrorKind::RecordTooLarge {
                                len,
                                max: limits.max_record_bytes,
                            },
                            start,
                        ));
                    }
                    Record::Output(reader.take(len, "an output record")?.to_vec())
                }
                TAG_RESIZE => {
                    let rows = reader.u16("a resize record")?;
                    let cols = reader.u16("a resize record")?;
                    if !limits.dimensions_ok(rows, cols) {
                        return Err(TraceError::new(
                            TraceErrorKind::InvalidDimensions { rows, cols },
                            start,
                        ));
                    }
                    Record::Resize { rows, cols }
                }
                tag => {
                    return Err(TraceError::new(
                        TraceErrorKind::UnknownRecordKind { tag },
                        start,
                    ))
                }
            });
        }
        Ok(Self {
            rows,
            cols,
            records,
        })
    }

    /// Encodes the trace in the file format.
    ///
    /// # Errors
    ///
    /// Returns a [`TraceError`] when a size or a record is outside `limits`,
    /// so the encoder never writes a file the parser would reject.
    pub fn encode(&self, limits: &Limits) -> Result<Vec<u8>, TraceError> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        if !limits.dimensions_ok(self.rows, self.cols) {
            return Err(TraceError::new(
                TraceErrorKind::InvalidDimensions {
                    rows: self.rows,
                    cols: self.cols,
                },
                out.len(),
            ));
        }
        out.extend_from_slice(&self.rows.to_le_bytes());
        out.extend_from_slice(&self.cols.to_le_bytes());
        if self.records.len() > limits.max_records {
            return Err(TraceError::new(
                TraceErrorKind::TooManyRecords {
                    max: limits.max_records,
                },
                out.len(),
            ));
        }
        for record in &self.records {
            match record {
                Record::Output(bytes) => {
                    if bytes.len() > limits.max_record_bytes {
                        return Err(TraceError::new(
                            TraceErrorKind::RecordTooLarge {
                                len: bytes.len(),
                                max: limits.max_record_bytes,
                            },
                            out.len(),
                        ));
                    }
                    let len = u32::try_from(bytes.len()).map_err(|_| {
                        TraceError::new(
                            TraceErrorKind::RecordTooLarge {
                                len: bytes.len(),
                                max: limits.max_record_bytes,
                            },
                            out.len(),
                        )
                    })?;
                    out.push(TAG_OUTPUT);
                    out.extend_from_slice(&len.to_le_bytes());
                    out.extend_from_slice(bytes);
                }
                Record::Resize { rows, cols } => {
                    if !limits.dimensions_ok(*rows, *cols) {
                        return Err(TraceError::new(
                            TraceErrorKind::InvalidDimensions {
                                rows: *rows,
                                cols: *cols,
                            },
                            out.len(),
                        ));
                    }
                    out.push(TAG_RESIZE);
                    out.extend_from_slice(&rows.to_le_bytes());
                    out.extend_from_slice(&cols.to_le_bytes());
                }
            }
        }
        if out.len() > limits.max_trace_bytes {
            return Err(TraceError::new(
                TraceErrorKind::TraceTooLarge {
                    len: out.len(),
                    max: limits.max_trace_bytes,
                },
                0,
            ));
        }
        debug_assert!(out.len() >= HEADER_BYTES);
        Ok(out)
    }
}
