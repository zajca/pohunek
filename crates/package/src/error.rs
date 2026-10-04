//! Typed archive failures.
//!
//! No variant carries archive content: entry names, file bytes and decoder
//! messages stay out of diagnostics so a hostile archive cannot inject text
//! into logs or terminals. Entries are identified by their position only.

// Rust guideline compliant 2026-10-04

use thiserror::Error;

/// Why one archive entry was rejected.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum EntryRejection {
    /// The path is empty.
    #[error("path is empty")]
    PathEmpty,
    /// The path exceeds the configured length limit.
    #[error("path exceeds the length limit")]
    PathTooLong,
    /// The path is not valid UTF-8.
    #[error("path is not valid UTF-8")]
    PathNotUtf8,
    /// The path starts with `/`.
    #[error("path is absolute")]
    PathAbsolute,
    /// The path has an empty segment (`//` or a trailing `/`).
    #[error("path has an empty segment")]
    PathEmptySegment,
    /// The path has a `.` or `..` segment.
    #[error("path has a dot segment")]
    PathDotSegment,
    /// The path has a character outside ASCII letters, digits, `.`, `_`, `-`
    /// and the `/` separator.
    #[error("path has a forbidden character")]
    PathCharacter,
    /// Two entries have the same path.
    #[error("path is duplicated")]
    Duplicate,
    /// Two entries differ only by ASCII letter case.
    #[error("path collides with another entry when case is folded")]
    CaseCollision,
    /// A path is both a file and a directory prefix of another entry.
    #[error("path is both a file and a directory")]
    FileDirectoryConflict,
    /// Entries are not in strictly ascending byte order.
    #[error("entries are not sorted")]
    Unsorted,
    /// The entry is a symbolic link.
    #[error("entry is a symbolic link")]
    Symlink,
    /// The entry is a hard link.
    #[error("entry is a hard link")]
    Hardlink,
    /// The entry is a device, fifo or other special file.
    #[error("entry is a special file")]
    SpecialFile,
    /// The entry is a directory; the format stores regular files only.
    #[error("entry is a directory")]
    Directory,
    /// The entry is a PAX or GNU extension header.
    #[error("entry is an extension header")]
    ExtensionHeader,
    /// The entry has a type flag the format does not define.
    #[error("entry has an unsupported type")]
    UnsupportedType,
    /// The header differs from the canonical header for this entry.
    #[error("header is not canonical")]
    NonCanonicalHeader,
    /// The padding after the file data is not zero-filled.
    #[error("padding is not canonical")]
    NonCanonicalPadding,
    /// The file exceeds the per-file size limit.
    #[error("file exceeds the size limit")]
    FileTooLarge,
}

/// Why building or reading an archive failed.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ArchiveError {
    /// The compressed archive exceeds the size limit.
    #[error("compressed archive exceeds {limit} bytes")]
    CompressedTooLarge {
        /// The limit that was exceeded.
        limit: u64,
    },
    /// The archive digest differs from the expected digest.
    #[error("archive digest does not match the expected digest")]
    DigestMismatch,
    /// The zstd frame header is invalid or unsupported.
    #[error("zstd frame header is invalid or unsupported")]
    ZstdHeader,
    /// The zstd frame declares a window above the limit.
    #[error("zstd window exceeds {limit} bytes")]
    ZstdWindow {
        /// The limit that was exceeded.
        limit: u64,
    },
    /// The zstd frame body is corrupt or truncated.
    #[error("zstd frame is corrupt or truncated")]
    ZstdCorrupt,
    /// The zstd content checksum is absent or does not match.
    #[error("zstd content checksum is missing or wrong")]
    ZstdChecksum,
    /// Bytes follow the single zstd frame.
    #[error("data follows the zstd frame")]
    TrailingCompressedData,
    /// The decompressed stream exceeds the size limit.
    #[error("decompressed archive exceeds {limit} bytes")]
    ExpandedTooLarge {
        /// The limit that was exceeded.
        limit: u64,
    },
    /// The decompressed stream exceeds the expansion-ratio limit.
    #[error("decompressed archive exceeds the expansion ratio of {limit}")]
    ExpansionRatio {
        /// The ratio that was exceeded.
        limit: u64,
    },
    /// The archive has more files than the limit.
    #[error("archive has more than {limit} files")]
    TooManyFiles {
        /// The limit that was exceeded.
        limit: usize,
    },
    /// The tar stream ends inside an entry or before the end marker.
    #[error("tar stream is truncated")]
    Truncated,
    /// Bytes follow the tar end marker.
    #[error("data follows the tar end marker")]
    TrailingData,
    /// One entry was rejected.
    #[error("entry {index} rejected: {reason}")]
    Entry {
        /// Zero-based position of the entry.
        index: usize,
        /// Why the entry was rejected.
        reason: EntryRejection,
    },
    /// The computed digest is not a valid package digest.
    #[error("computed archive digest is not a valid package digest")]
    DigestEncoding,
}
