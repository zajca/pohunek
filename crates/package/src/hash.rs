//! SHA-256 helpers shared by the manifest and the registry.

// Rust guideline compliant 2026-10-04

use std::io::Read;

use sha2::{Digest as _, Sha256};

/// Lowercase hexadecimal digits.
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// Encodes `bytes` as lowercase hexadecimal.
pub(crate) fn lower_hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(HEX_DIGITS[usize::from(byte & 0x0f)]));
    }
    text
}

/// SHA-256 of `bytes` as lowercase hexadecimal.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    lower_hex(&Sha256::digest(bytes))
}

/// Size of the buffer a streamed hash reads through: 64 KiB.
///
/// Large enough that syscall overhead is negligible for the 16 MiB file limit,
/// small enough to stay off the heap-pressure radar of a daemon.
const STREAM_BUFFER_BYTES: usize = 64 * 1024;

/// What a bounded streamed hash observed.
pub(crate) struct StreamHash {
    /// Bytes read, at most `limit + 1`.
    pub(crate) len: u64,
    /// SHA-256 of the bytes read, lowercase hexadecimal.
    pub(crate) hex: String,
}

/// Hashes at most `limit + 1` bytes of `reader`.
///
/// Reading one byte past `limit` lets the caller tell a file that grew from one
/// of exactly the expected size without buffering the file.
pub(crate) fn stream_sha256(reader: &mut impl Read, limit: u64) -> std::io::Result<StreamHash> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; STREAM_BUFFER_BYTES];
    let mut remaining = limit.saturating_add(1);
    let mut len = 0_u64;
    while remaining > 0 {
        let want = usize::try_from(remaining).map_or(buffer.len(), |left| left.min(buffer.len()));
        let read = reader.read(&mut buffer[..want])?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        let read = u64::try_from(read).unwrap_or(u64::MAX);
        len += read;
        remaining -= read;
    }
    Ok(StreamHash {
        len,
        hex: lower_hex(&hasher.finalize()),
    })
}
