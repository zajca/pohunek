//! Strict USTAR writer and reader over a single zstd frame.
//!
//! The canonical stream is: for each file in ascending path order one header
//! block, the file bytes and zero padding to a block boundary; then two zero
//! blocks and nothing else. The reader rebuilds the canonical header for every
//! entry it parses and requires the actual header to match byte for byte.

// Rust guideline compliant 2026-10-04

use std::collections::HashSet;

use protocol::PackageDigest;
use sha2::{Digest as _, Sha256};

use crate::canonical::{
    encode_header, mode_is_executable, parse_octal, size_field, BLOCK_BYTES, END_MARKER_BYTES,
    NAME_BYTES, TYPEFLAG_OFFSET, TYPEFLAG_REGULAR,
};
use crate::compression;
use crate::error::{ArchiveError, EntryRejection};
use crate::limits::Limits;

/// One regular file of a package archive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveEntry {
    /// Relative path using `/` separators. Segments consist of ASCII letters,
    /// digits, `.`, `_` and `-`; `.` and `..` segments are invalid.
    pub path: String,
    /// File contents.
    pub contents: Vec<u8>,
    /// Whether the file is executable (canonical mode `0755` instead of `0644`).
    pub executable: bool,
}

/// An archive that passed every check of the strict reader.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedArchive {
    digest: PackageDigest,
    entries: Vec<ArchiveEntry>,
}

impl VerifiedArchive {
    /// Digest of the archive bytes this value was read from.
    #[must_use]
    pub fn digest(&self) -> &PackageDigest {
        &self.digest
    }

    /// Files in canonical (ascending path) order.
    #[must_use]
    pub fn entries(&self) -> &[ArchiveEntry] {
        &self.entries
    }

    /// Consumes the archive and returns its files.
    #[must_use]
    pub fn into_entries(self) -> Vec<ArchiveEntry> {
        self.entries
    }
}

/// Builds the canonical archive bytes for `entries`.
///
/// Entries may be given in any order; the output is sorted by path. The result
/// is re-read with the strict reader under the same `limits`, so a returned
/// archive is always one [`read_archive`] accepts.
///
/// # Errors
///
/// Returns the first invalid entry (path rules, duplicates, case collisions,
/// size limits) or a limit violation of the finished archive.
pub fn build_archive(entries: &[ArchiveEntry], limits: &Limits) -> Result<Vec<u8>, ArchiveError> {
    if entries.len() > limits.max_files {
        return Err(ArchiveError::TooManyFiles {
            limit: limits.max_files,
        });
    }
    for (index, entry) in entries.iter().enumerate() {
        validate_path(entry.path.as_bytes(), limits)
            .map_err(|reason| entry_error(index, reason))?;
    }
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by(|&left, &right| entries[left].path.cmp(&entries[right].path));

    let mut paths = PathIndex::default();
    let mut previous: Option<&str> = None;
    let mut tar = Vec::new();
    for &index in &order {
        let entry = &entries[index];
        if previous == Some(entry.path.as_str()) {
            return Err(entry_error(index, EntryRejection::Duplicate));
        }
        paths
            .insert(&entry.path)
            .map_err(|reason| entry_error(index, reason))?;
        previous = Some(&entry.path);

        let size = u64::try_from(entry.contents.len()).unwrap_or(u64::MAX);
        if size > limits.max_file_bytes {
            return Err(entry_error(index, EntryRejection::FileTooLarge));
        }
        let header = encode_header(entry.path.as_bytes(), size, entry.executable)
            .map_err(|reason| entry_error(index, reason))?;
        tar.extend_from_slice(&header);
        tar.extend_from_slice(&entry.contents);
        tar.resize(tar.len() + padding_len(entry.contents.len()), 0);
    }
    tar.resize(tar.len() + END_MARKER_BYTES, 0);
    if u64::try_from(tar.len()).unwrap_or(u64::MAX) > limits.max_expanded_bytes {
        return Err(ArchiveError::ExpandedTooLarge {
            limit: limits.max_expanded_bytes,
        });
    }

    let bytes = compression::encode(&tar);
    read_archive(&bytes, limits)?;
    Ok(bytes)
}

/// Reads archive bytes strictly into memory.
///
/// # Errors
///
/// Returns an [`ArchiveError`] for any limit violation, malformed zstd frame
/// or tar stream, or entry that is not in canonical form.
pub fn read_archive(bytes: &[u8], limits: &Limits) -> Result<VerifiedArchive, ArchiveError> {
    read_checked(bytes, None, limits)
}

/// Reads archive bytes after checking them against `expected`.
///
/// The digest comparison happens before any decompression or parsing, so a
/// tampered archive is never interpreted.
///
/// # Errors
///
/// Returns [`ArchiveError::DigestMismatch`] when the digest differs, otherwise
/// the errors of [`read_archive`].
pub fn read_archive_with_digest(
    bytes: &[u8],
    expected: &PackageDigest,
    limits: &Limits,
) -> Result<VerifiedArchive, ArchiveError> {
    read_checked(bytes, Some(expected), limits)
}

fn read_checked(
    bytes: &[u8],
    expected: Option<&PackageDigest>,
    limits: &Limits,
) -> Result<VerifiedArchive, ArchiveError> {
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limits.max_compressed_bytes {
        return Err(ArchiveError::CompressedTooLarge {
            limit: limits.max_compressed_bytes,
        });
    }
    let digest = digest_of(bytes)?;
    if expected.is_some_and(|expected| *expected != digest) {
        return Err(ArchiveError::DigestMismatch);
    }
    let tar = compression::decode(bytes, limits)?;
    let entries = parse_tar(&tar, limits)?;
    Ok(VerifiedArchive { digest, entries })
}

/// Lowercase hexadecimal digits, as the `sha256:` digest syntax requires.
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

fn digest_of(bytes: &[u8]) -> Result<PackageDigest, ArchiveError> {
    let hash = Sha256::digest(bytes);
    let mut text = String::from("sha256:");
    for byte in hash {
        text.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(HEX_DIGITS[usize::from(byte & 0x0f)]));
    }
    PackageDigest::parse(&text).map_err(|_cause| ArchiveError::DigestEncoding)
}

fn entry_error(index: usize, reason: EntryRejection) -> ArchiveError {
    ArchiveError::Entry { index, reason }
}

/// Zero bytes that pad `len` file bytes to a block boundary.
fn padding_len(len: usize) -> usize {
    (BLOCK_BYTES - len % BLOCK_BYTES) % BLOCK_BYTES
}

fn parse_tar(tar: &[u8], limits: &Limits) -> Result<Vec<ArchiveEntry>, ArchiveError> {
    let mut entries: Vec<ArchiveEntry> = Vec::new();
    let mut paths = PathIndex::default();
    let mut offset = 0_usize;
    loop {
        let block: &[u8; BLOCK_BYTES] = tar
            .get(offset..offset + BLOCK_BYTES)
            .and_then(|slice| slice.try_into().ok())
            .ok_or(ArchiveError::Truncated)?;
        if block.iter().all(|&byte| byte == 0) {
            return finish_tar(&tar[offset..], entries);
        }
        let index = entries.len();
        if index >= limits.max_files {
            return Err(ArchiveError::TooManyFiles {
                limit: limits.max_files,
            });
        }
        let reject = |reason| entry_error(index, reason);

        check_type(block[TYPEFLAG_OFFSET]).map_err(reject)?;
        let name_len = block[..NAME_BYTES]
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(NAME_BYTES);
        let name = &block[..name_len];
        let path = validate_path(name, limits).map_err(reject)?;
        match entries.last() {
            Some(last) if last.path == path => {
                return Err(reject(EntryRejection::Duplicate));
            }
            Some(last) if last.path.as_str() > path => {
                return Err(reject(EntryRejection::Unsorted));
            }
            _ => {}
        }
        paths.insert(path).map_err(reject)?;

        let size = parse_octal(size_field(block))
            .ok_or_else(|| reject(EntryRejection::NonCanonicalHeader))?;
        if size > limits.max_file_bytes {
            return Err(reject(EntryRejection::FileTooLarge));
        }
        let executable = mode_is_executable(block);
        let canonical = encode_header(name, size, executable).map_err(reject)?;
        if canonical != *block {
            return Err(reject(EntryRejection::NonCanonicalHeader));
        }

        let size = usize::try_from(size).map_err(|_cause| reject(EntryRejection::FileTooLarge))?;
        let data_start = offset + BLOCK_BYTES;
        let data_end = data_start
            .checked_add(size)
            .ok_or(ArchiveError::Truncated)?;
        let padded_end = data_end + padding_len(size);
        let region = tar
            .get(data_start..padded_end)
            .ok_or(ArchiveError::Truncated)?;
        if region[size..].iter().any(|&byte| byte != 0) {
            return Err(reject(EntryRejection::NonCanonicalPadding));
        }
        entries.push(ArchiveEntry {
            path: path.to_owned(),
            contents: region[..size].to_vec(),
            executable,
        });
        offset = padded_end;
    }
}

/// Checks that exactly the two-block end marker remains.
fn finish_tar(rest: &[u8], entries: Vec<ArchiveEntry>) -> Result<Vec<ArchiveEntry>, ArchiveError> {
    match rest.len().cmp(&END_MARKER_BYTES) {
        std::cmp::Ordering::Less => Err(ArchiveError::Truncated),
        std::cmp::Ordering::Equal if rest.iter().all(|&byte| byte == 0) => Ok(entries),
        std::cmp::Ordering::Greater | std::cmp::Ordering::Equal => Err(ArchiveError::TrailingData),
    }
}

/// Rejects every type flag except a regular file.
fn check_type(typeflag: u8) -> Result<(), EntryRejection> {
    match typeflag {
        TYPEFLAG_REGULAR => Ok(()),
        b'1' => Err(EntryRejection::Hardlink),
        b'2' => Err(EntryRejection::Symlink),
        b'3' | b'4' | b'6' => Err(EntryRejection::SpecialFile),
        b'5' => Err(EntryRejection::Directory),
        // PAX per-file and global headers, GNU long name/link, GNU sparse and
        // the other GNU/star extension records.
        b'x' | b'g' | b'L' | b'K' | b'S' | b'X' | b'D' | b'M' | b'N' | b'V' => {
            Err(EntryRejection::ExtensionHeader)
        }
        _ => Err(EntryRejection::UnsupportedType),
    }
}

/// Validates one archive path and returns it as text.
fn validate_path<'a>(raw: &'a [u8], limits: &Limits) -> Result<&'a str, EntryRejection> {
    if raw.is_empty() {
        return Err(EntryRejection::PathEmpty);
    }
    if raw.len() > limits.max_path_bytes.min(NAME_BYTES) {
        return Err(EntryRejection::PathTooLong);
    }
    let path = std::str::from_utf8(raw).map_err(|_cause| EntryRejection::PathNotUtf8)?;
    if path.starts_with('/') {
        return Err(EntryRejection::PathAbsolute);
    }
    for segment in path.split('/') {
        if segment.is_empty() {
            return Err(EntryRejection::PathEmptySegment);
        }
        if segment == "." || segment == ".." {
            return Err(EntryRejection::PathDotSegment);
        }
        if !segment.bytes().all(is_path_byte) {
            return Err(EntryRejection::PathCharacter);
        }
    }
    Ok(path)
}

/// Bytes allowed inside a path segment.
///
/// ASCII only: no case folding beyond ASCII, no Unicode normalization forms
/// and no control characters or separators that differ between host
/// filesystems.
fn is_path_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
}

/// Tracks accepted paths to catch case-fold and file/directory collisions.
#[derive(Default)]
struct PathIndex {
    files: HashSet<String>,
    directories: HashSet<String>,
}

impl PathIndex {
    fn insert(&mut self, path: &str) -> Result<(), EntryRejection> {
        let folded = path.to_ascii_lowercase();
        if self.files.contains(&folded) {
            return Err(EntryRejection::CaseCollision);
        }
        if self.directories.contains(&folded) {
            return Err(EntryRejection::FileDirectoryConflict);
        }
        let prefixes: Vec<&str> = folded
            .match_indices('/')
            .map(|(at, _)| &folded[..at])
            .collect();
        if prefixes.iter().any(|prefix| self.files.contains(*prefix)) {
            return Err(EntryRejection::FileDirectoryConflict);
        }
        self.directories
            .extend(prefixes.into_iter().map(str::to_owned));
        self.files.insert(folded);
        Ok(())
    }
}
