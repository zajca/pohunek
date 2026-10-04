//! Single-frame zstd encoding and bounded decoding.
//!
//! Both directions use the pure-Rust `ruzstd` crate so the builder output is
//! reproducible from a pinned dependency version and release builds stay free
//! of a C toolchain.

// Rust guideline compliant 2026-10-04

use std::io::Read as _;

use ruzstd::decoding::errors::FrameDecoderError;
use ruzstd::decoding::StreamingDecoder;
use ruzstd::encoding::{compress_to_vec, CompressionLevel};

use crate::error::ArchiveError;
use crate::limits::Limits;

/// Size of the scratch buffer the decoder drains into.
const DECODE_CHUNK_BYTES: usize = 16 * 1024;

/// Compresses `tar` into one zstd frame with a content checksum.
///
/// The compressor is single-threaded and level-fixed, so the same input always
/// yields the same bytes for a given `ruzstd` version.
pub(crate) fn encode(tar: &[u8]) -> Vec<u8> {
    compress_to_vec(tar, CompressionLevel::Fastest)
}

/// Decompresses exactly one zstd frame, refusing to produce more than the
/// size and expansion-ratio limits allow.
///
/// The output buffer never grows past the limit, so a decompression bomb costs
/// at most `limit` bytes of memory before it is rejected.
pub(crate) fn decode(bytes: &[u8], limits: &Limits) -> Result<Vec<u8>, ArchiveError> {
    let compressed_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let ratio_cap = compressed_len.saturating_mul(limits.max_expansion_ratio);
    let cap = limits.max_expanded_bytes.min(ratio_cap);

    let mut source = bytes;
    let mut decoder =
        StreamingDecoder::new_with_max_window_size(&mut source, limits.max_window_bytes)
            .map_err(|error| header_error(&error, limits))?;

    let mut output = Vec::new();
    let mut chunk = [0_u8; DECODE_CHUNK_BYTES];
    loop {
        let read = decoder
            .read(&mut chunk)
            .map_err(|_cause| ArchiveError::ZstdCorrupt)?;
        if read == 0 {
            break;
        }
        let produced = u64::try_from(output.len() + read).unwrap_or(u64::MAX);
        if produced > cap {
            return Err(if cap < limits.max_expanded_bytes {
                ArchiveError::ExpansionRatio {
                    limit: limits.max_expansion_ratio,
                }
            } else {
                ArchiveError::ExpandedTooLarge {
                    limit: limits.max_expanded_bytes,
                }
            });
        }
        output.extend_from_slice(&chunk[..read]);
    }

    // ruzstd reads the stored checksum but does not compare it.
    let stored = decoder.decoder.get_checksum_from_data();
    let computed = decoder.decoder.get_calculated_checksum();
    if stored.is_none() || stored != computed {
        return Err(ArchiveError::ZstdChecksum);
    }
    drop(decoder);
    if !source.is_empty() {
        return Err(ArchiveError::TrailingCompressedData);
    }
    Ok(output)
}

fn header_error(error: &FrameDecoderError, limits: &Limits) -> ArchiveError {
    match error {
        FrameDecoderError::WindowSizeTooBig { .. } => ArchiveError::ZstdWindow {
            limit: limits.max_window_bytes,
        },
        FrameDecoderError::ReadFrameHeaderError(_)
        | FrameDecoderError::FrameHeaderError(_)
        | FrameDecoderError::FailedToInitialize(_) => ArchiveError::ZstdHeader,
        _ => ArchiveError::ZstdCorrupt,
    }
}
