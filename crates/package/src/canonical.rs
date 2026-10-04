//! Canonical USTAR header encoding.
//!
//! The reader accepts a header only when it equals the bytes this module
//! produces for the same name, size and executable flag, so every metadata
//! field is pinned by one function instead of by per-field checks.

// Rust guideline compliant 2026-10-04

use crate::error::EntryRejection;

/// Size of a tar block in bytes; headers and file data are block-aligned.
pub(crate) const BLOCK_BYTES: usize = 512;

/// Size of the end-of-archive marker: two zero blocks.
pub(crate) const END_MARKER_BYTES: usize = 2 * BLOCK_BYTES;

/// Capacity of the USTAR `name` field.
pub(crate) const NAME_BYTES: usize = 100;

/// Type flag of a regular file.
pub(crate) const TYPEFLAG_REGULAR: u8 = b'0';

const MODE_OFFSET: usize = 100;
const UID_OFFSET: usize = 108;
const GID_OFFSET: usize = 116;
const SIZE_OFFSET: usize = 124;
const SIZE_FIELD_BYTES: usize = 12;
const MTIME_OFFSET: usize = 136;
const CHECKSUM_OFFSET: usize = 148;
const CHECKSUM_FIELD_BYTES: usize = 8;
/// Offset of the type flag byte.
pub(crate) const TYPEFLAG_OFFSET: usize = 156;
const MAGIC_OFFSET: usize = 257;
const VERSION_OFFSET: usize = 263;
const DEVMAJOR_OFFSET: usize = 329;
const DEVMINOR_OFFSET: usize = 337;

/// Width of an 8-byte numeric field: seven octal digits and a NUL.
const SHORT_FIELD_BYTES: usize = 8;

/// Mode of a non-executable file.
const MODE_FILE: u64 = 0o644;
/// Mode of an executable file.
const MODE_EXECUTABLE: u64 = 0o755;

/// POSIX ustar magic including its NUL terminator.
const MAGIC: &[u8; 6] = b"ustar\0";
/// POSIX ustar version field.
const VERSION: &[u8; 2] = b"00";

/// Largest value the 11-digit octal size field can hold.
const MAX_SIZE_FIELD_VALUE: u64 = 0o77_777_777_777;

/// Writes `value` as zero-padded octal digits followed by a NUL, filling
/// `field`. The caller guarantees the value fits.
fn write_octal(field: &mut [u8], value: u64) {
    let digits = field.len() - 1;
    let mut remaining = value;
    for slot in field[..digits].iter_mut().rev() {
        // Masked to three bits, so the cast cannot truncate.
        let digit = (remaining & 0b111) as u8;
        *slot = b'0' + digit;
        remaining >>= 3;
    }
    field[digits] = 0;
}

/// Parses a NUL-terminated octal numeric field; `None` if it holds anything
/// else.
pub(crate) fn parse_octal(field: &[u8]) -> Option<u64> {
    let (digits, terminator) = field.split_at(field.len().checked_sub(1)?);
    if terminator != [0] || digits.is_empty() {
        return None;
    }
    digits.iter().try_fold(0_u64, |value, &byte| {
        if (b'0'..=b'7').contains(&byte) {
            value.checked_mul(8)?.checked_add(u64::from(byte - b'0'))
        } else {
            None
        }
    })
}

/// Size field of a header block.
pub(crate) fn size_field(block: &[u8; BLOCK_BYTES]) -> &[u8] {
    &block[SIZE_OFFSET..SIZE_OFFSET + SIZE_FIELD_BYTES]
}

/// Whether the header's mode field is the canonical executable mode.
pub(crate) fn mode_is_executable(block: &[u8; BLOCK_BYTES]) -> bool {
    parse_octal(&block[MODE_OFFSET..MODE_OFFSET + SHORT_FIELD_BYTES]) == Some(MODE_EXECUTABLE)
}

/// Builds the canonical header block of one regular file.
///
/// Every field other than name, size and the executable bit is fixed: owner
/// ids are zero, the owner and group names are empty, the modification time is
/// the epoch and the device numbers are zero.
pub(crate) fn encode_header(
    name: &[u8],
    size: u64,
    executable: bool,
) -> Result<[u8; BLOCK_BYTES], EntryRejection> {
    if name.len() > NAME_BYTES {
        return Err(EntryRejection::PathTooLong);
    }
    if size > MAX_SIZE_FIELD_VALUE {
        return Err(EntryRejection::FileTooLarge);
    }
    let mut block = [0_u8; BLOCK_BYTES];
    block[..name.len()].copy_from_slice(name);
    let mode = if executable {
        MODE_EXECUTABLE
    } else {
        MODE_FILE
    };
    write_octal(
        &mut block[MODE_OFFSET..MODE_OFFSET + SHORT_FIELD_BYTES],
        mode,
    );
    write_octal(&mut block[UID_OFFSET..UID_OFFSET + SHORT_FIELD_BYTES], 0);
    write_octal(&mut block[GID_OFFSET..GID_OFFSET + SHORT_FIELD_BYTES], 0);
    write_octal(
        &mut block[SIZE_OFFSET..SIZE_OFFSET + SIZE_FIELD_BYTES],
        size,
    );
    write_octal(&mut block[MTIME_OFFSET..MTIME_OFFSET + SIZE_FIELD_BYTES], 0);
    block[TYPEFLAG_OFFSET] = TYPEFLAG_REGULAR;
    block[MAGIC_OFFSET..MAGIC_OFFSET + MAGIC.len()].copy_from_slice(MAGIC);
    block[VERSION_OFFSET..VERSION_OFFSET + VERSION.len()].copy_from_slice(VERSION);
    write_octal(
        &mut block[DEVMAJOR_OFFSET..DEVMAJOR_OFFSET + SHORT_FIELD_BYTES],
        0,
    );
    write_octal(
        &mut block[DEVMINOR_OFFSET..DEVMINOR_OFFSET + SHORT_FIELD_BYTES],
        0,
    );

    // The checksum is the byte sum of the block with the checksum field read
    // as spaces; it is stored as six octal digits, a NUL and a space.
    block[CHECKSUM_OFFSET..CHECKSUM_OFFSET + CHECKSUM_FIELD_BYTES].fill(b' ');
    let checksum: u64 = block.iter().map(|&byte| u64::from(byte)).sum();
    let field = &mut block[CHECKSUM_OFFSET..CHECKSUM_OFFSET + CHECKSUM_FIELD_BYTES];
    write_octal(&mut field[..CHECKSUM_FIELD_BYTES - 1], checksum);
    field[CHECKSUM_FIELD_BYTES - 1] = b' ';
    Ok(block)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn octal_round_trips() {
        let mut field = [0_u8; SIZE_FIELD_BYTES];
        write_octal(&mut field, 0o1234);
        assert_eq!(&field, b"00000001234\0");
        assert_eq!(parse_octal(&field), Some(0o1234));
    }

    #[test]
    fn octal_rejects_non_octal_digits_and_missing_terminator() {
        assert_eq!(parse_octal(b"0000008\0"), None);
        assert_eq!(parse_octal(b"00000010"), None);
        assert_eq!(parse_octal(b"000 0010\0"), None);
        assert_eq!(parse_octal(b"\0"), None);
    }

    #[test]
    fn header_checksum_matches_gnu_ustar_convention() {
        let block = encode_header(b"a", 0, false).expect("valid header");
        let stored = parse_octal(&[&block[148..154], &[0]].concat()).expect("octal checksum");
        let mut copy = block;
        copy[148..156].fill(b' ');
        let sum: u64 = copy.iter().map(|&byte| u64::from(byte)).sum();
        assert_eq!(stored, sum);
        assert_eq!(block[154], 0);
        assert_eq!(block[155], b' ');
    }

    #[test]
    fn oversized_name_and_size_are_rejected() {
        assert_eq!(
            encode_header(&[b'a'; NAME_BYTES + 1], 0, false),
            Err(EntryRejection::PathTooLong)
        );
        assert_eq!(
            encode_header(b"a", MAX_SIZE_FIELD_VALUE + 1, false),
            Err(EntryRejection::FileTooLarge)
        );
    }
}
