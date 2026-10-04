// Rust guideline compliant 2026-10-04

//! Behavioural tests of the canonical archive builder and strict reader.
//!
//! Hostile archives are forged byte by byte so each rejection rule is pinned by
//! its own test; the builder is never used to produce a malformed archive.

use package::{
    build_archive, read_archive, read_archive_with_digest, ArchiveEntry, ArchiveError,
    EntryRejection, Limits, PackageDigest,
};
use ruzstd::encoding::{compress_to_vec, CompressionLevel};
use sha2::{Digest as _, Sha256};

const BLOCK: usize = 512;
const NAME_END: usize = 100;
const MODE: usize = 100;
const UID: usize = 108;
const GID: usize = 116;
const MTIME: usize = 136;
const CHECKSUM: usize = 148;
const TYPEFLAG: usize = 156;
const LINKNAME: usize = 157;
const UNAME: usize = 265;
const GNAME: usize = 297;
const PREFIX: usize = 345;

fn entry(path: &str, contents: &[u8]) -> ArchiveEntry {
    ArchiveEntry {
        path: path.to_owned(),
        contents: contents.to_vec(),
        executable: false,
    }
}

fn sample() -> Vec<ArchiveEntry> {
    vec![
        entry("runtime.toml", b"schema = 1\n"),
        entry("detect/default.toml", b"[detect]\n"),
        entry("LICENSE", b"MIT\n"),
        ArchiveEntry {
            path: "integration/assets/hook.sh".to_owned(),
            contents: b"#!/bin/sh\n".to_vec(),
            executable: true,
        },
    ]
}

/// Limits that do not interfere with forged archives, which compress far
/// better than real packages.
fn lenient() -> Limits {
    Limits {
        max_expansion_ratio: u64::MAX,
        ..Limits::DEFAULT
    }
}

fn rejection(result: Result<package::VerifiedArchive, ArchiveError>) -> EntryRejection {
    match result {
        Err(ArchiveError::Entry { reason, .. }) => reason,
        other => panic!("expected an entry rejection, got {other:?}"),
    }
}

fn octal(field: &mut [u8], value: u64) {
    let digits = field.len() - 1;
    let text = format!("{value:0digits$o}");
    field[..digits].copy_from_slice(text.as_bytes());
    field[digits] = 0;
}

fn refresh_checksum(block: &mut [u8; BLOCK]) {
    block[CHECKSUM..CHECKSUM + 8].fill(b' ');
    let sum: u64 = block.iter().map(|&byte| u64::from(byte)).sum();
    octal(&mut block[CHECKSUM..CHECKSUM + 7], sum);
    block[CHECKSUM + 7] = b' ';
}

/// A canonical header for a regular file, independent of the crate under test.
fn header(name: &[u8], size: u64, typeflag: u8) -> [u8; BLOCK] {
    let mut block = [0_u8; BLOCK];
    block[..name.len()].copy_from_slice(name);
    octal(&mut block[MODE..MODE + 8], 0o644);
    octal(&mut block[UID..UID + 8], 0);
    octal(&mut block[GID..GID + 8], 0);
    octal(&mut block[124..136], size);
    octal(&mut block[MTIME..MTIME + 12], 0);
    block[TYPEFLAG] = typeflag;
    block[257..263].copy_from_slice(b"ustar\0");
    block[263..265].copy_from_slice(b"00");
    octal(&mut block[329..337], 0);
    octal(&mut block[337..345], 0);
    refresh_checksum(&mut block);
    block
}

struct Forged(Vec<u8>);

impl Forged {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn block(mut self, block: &[u8; BLOCK], data: &[u8]) -> Self {
        self.0.extend_from_slice(block);
        self.0.extend_from_slice(data);
        let pad = (BLOCK - data.len() % BLOCK) % BLOCK;
        self.0.resize(self.0.len() + pad, 0);
        self
    }

    fn file(self, name: &[u8], data: &[u8]) -> Self {
        let block = header(name, data.len() as u64, b'0');
        self.block(&block, data)
    }

    fn end(mut self) -> Vec<u8> {
        self.0.resize(self.0.len() + 2 * BLOCK, 0);
        self.0
    }
}

fn zstd(tar: &[u8]) -> Vec<u8> {
    compress_to_vec(tar, CompressionLevel::Fastest)
}

fn read_forged(tar: &[u8]) -> Result<package::VerifiedArchive, ArchiveError> {
    read_archive(&zstd(tar), &lenient())
}

fn mutated(
    mutate: impl FnOnce(&mut [u8; BLOCK]),
) -> Result<package::VerifiedArchive, ArchiveError> {
    let mut block = header(b"runtime.toml", 3, b'0');
    mutate(&mut block);
    refresh_checksum(&mut block);
    read_forged(&Forged::new().block(&block, b"abc").end())
}

fn tar_len(entries: &[ArchiveEntry]) -> u64 {
    let files: usize = entries
        .iter()
        .map(|entry| BLOCK + entry.contents.len().div_ceil(BLOCK) * BLOCK)
        .sum();
    (files + 2 * BLOCK) as u64
}

// ---------------------------------------------------------------- round trip

#[test]
fn round_trip_returns_sorted_entries_and_digest() {
    let bytes = build_archive(&sample(), &Limits::DEFAULT).expect("build");
    let archive = read_archive(&bytes, &Limits::DEFAULT).expect("read");

    let paths: Vec<&str> = archive.entries().iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "LICENSE",
            "detect/default.toml",
            "integration/assets/hook.sh",
            "runtime.toml"
        ]
    );
    let hook = &archive.entries()[2];
    assert_eq!(hook.contents, b"#!/bin/sh\n");
    assert!(hook.executable);
    assert!(!archive.entries()[0].executable);

    let expected = format!("sha256:{:x}", Sha256::digest(&bytes));
    assert_eq!(archive.digest().as_str(), expected);
}

#[test]
fn empty_and_zero_length_files_round_trip() {
    let entries = vec![entry("empty", b""), entry("full", &[7_u8; BLOCK])];
    let bytes = build_archive(&entries, &Limits::DEFAULT).expect("build");
    let archive = read_archive(&bytes, &Limits::DEFAULT).expect("read");
    assert_eq!(archive.into_entries(), entries);

    let none = build_archive(&[], &Limits::DEFAULT).expect("empty archive");
    assert!(read_archive(&none, &Limits::DEFAULT)
        .expect("read")
        .entries()
        .is_empty());
}

#[test]
fn digest_gate_runs_before_any_decompression() {
    let bytes = build_archive(&sample(), &Limits::DEFAULT).expect("build");
    let digest = read_archive(&bytes, &Limits::DEFAULT)
        .expect("read")
        .digest()
        .clone();
    read_archive_with_digest(&bytes, &digest, &Limits::DEFAULT).expect("matching digest");

    // Garbage is not zstd, yet the digest verdict comes first.
    let garbage = b"not an archive".to_vec();
    assert_eq!(
        read_archive_with_digest(&garbage, &digest, &Limits::DEFAULT).unwrap_err(),
        ArchiveError::DigestMismatch
    );
    let other = PackageDigest::parse(&format!("sha256:{}", "0".repeat(64))).expect("digest");
    assert_eq!(
        read_archive_with_digest(&bytes, &other, &Limits::DEFAULT).unwrap_err(),
        ArchiveError::DigestMismatch
    );
}

// --------------------------------------------------------------- determinism

#[test]
fn build_is_deterministic_and_independent_of_input_order() {
    let first = build_archive(&sample(), &Limits::DEFAULT).expect("build");
    let second = build_archive(&sample(), &Limits::DEFAULT).expect("build");
    let mut reversed = sample();
    reversed.reverse();
    let third = build_archive(&reversed, &Limits::DEFAULT).expect("build");
    assert_eq!(first, second);
    assert_eq!(first, third);
}

#[test]
fn canonical_bytes_are_pinned() {
    // A change here means the canonical archive bytes changed: a `ruzstd`
    // bump or a format edit. Every published package digest changes with it,
    // so update the pin only as a deliberate format decision.
    let bytes = build_archive(&sample(), &Limits::DEFAULT).expect("build");
    let digest = format!("{:x}", Sha256::digest(&bytes));
    assert_eq!(digest, PINNED_SAMPLE_DIGEST);
}

const PINNED_SAMPLE_DIGEST: &str =
    "133fcf87bb30d8c45900acf3d81f2dfa6cd1f0169bab76cabc53fa34f6b7cdbb";

#[test]
fn canonical_tar_headers_use_fixed_metadata() {
    let tar = ruzstd_decode(&build_archive(&sample(), &Limits::DEFAULT).expect("build"));
    let first = &tar[..BLOCK];
    assert_eq!(&first[..7], b"LICENSE");
    assert_eq!(&first[MODE..MODE + 8], b"0000644\0");
    assert_eq!(&first[UID..UID + 8], b"0000000\0");
    assert_eq!(&first[GID..GID + 8], b"0000000\0");
    assert_eq!(&first[MTIME..MTIME + 12], b"00000000000\0");
    assert_eq!(first[TYPEFLAG], b'0');
    assert_eq!(&first[257..263], b"ustar\0");
    assert!(first[UNAME..UNAME + 64].iter().all(|&b| b == 0));
    assert_eq!(&tar[tar.len() - 2 * BLOCK..], &[0_u8; 2 * BLOCK][..]);
}

fn ruzstd_decode(bytes: &[u8]) -> Vec<u8> {
    use std::io::Read as _;
    let mut source = bytes;
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(&mut source).expect("frame");
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).expect("decode");
    out
}

// ------------------------------------------------------------ path rejection

#[test]
fn reader_rejects_traversal_paths() {
    for name in [&b"../evil"[..], b"a/../b", b"a/.."] {
        let tar = Forged::new().file(name, b"x").end();
        assert_eq!(rejection(read_forged(&tar)), EntryRejection::PathDotSegment);
    }
}

#[test]
fn reader_rejects_absolute_paths() {
    let tar = Forged::new().file(b"/etc/passwd", b"x").end();
    assert_eq!(rejection(read_forged(&tar)), EntryRejection::PathAbsolute);
}

#[test]
fn reader_rejects_dot_segments() {
    for name in [&b"./a"[..], b"a/./b", b"."] {
        let tar = Forged::new().file(name, b"x").end();
        assert_eq!(rejection(read_forged(&tar)), EntryRejection::PathDotSegment);
    }
}

#[test]
fn reader_rejects_empty_paths_and_segments() {
    let tar = Forged::new().file(b"", b"x").end();
    assert_eq!(rejection(read_forged(&tar)), EntryRejection::PathEmpty);
    for name in [&b"a//b"[..], b"a/"] {
        let tar = Forged::new().file(name, b"x").end();
        assert_eq!(
            rejection(read_forged(&tar)),
            EntryRejection::PathEmptySegment
        );
    }
}

#[test]
fn reader_rejects_duplicate_paths() {
    let tar = Forged::new().file(b"a", b"1").file(b"a", b"2").end();
    assert_eq!(rejection(read_forged(&tar)), EntryRejection::Duplicate);
}

#[test]
fn reader_rejects_case_folding_collisions() {
    let tar = Forged::new()
        .file(b"Readme", b"1")
        .file(b"readme", b"2")
        .end();
    assert_eq!(rejection(read_forged(&tar)), EntryRejection::CaseCollision);
}

#[test]
fn reader_rejects_file_and_directory_conflicts() {
    let tar = Forged::new().file(b"a", b"1").file(b"a/b", b"2").end();
    assert_eq!(
        rejection(read_forged(&tar)),
        EntryRejection::FileDirectoryConflict
    );
    // A file whose folded name equals an earlier entry's directory prefix.
    let tar = Forged::new().file(b"X/y", b"1").file(b"x", b"2").end();
    assert_eq!(
        rejection(read_forged(&tar)),
        EntryRejection::FileDirectoryConflict
    );
}

#[test]
fn reader_rejects_non_utf8_names() {
    let tar = Forged::new().file(&[b'a', 0xff, b'b'], b"x").end();
    assert_eq!(rejection(read_forged(&tar)), EntryRejection::PathNotUtf8);
}

#[test]
fn reader_rejects_control_and_non_ascii_characters() {
    for name in [
        &b"a\x01b"[..],
        b"a\nb",
        b"a\x7fb",
        b"a b",
        b"a\\b",
        "caf\u{e9}".as_bytes(),
    ] {
        let tar = Forged::new().file(name, b"x").end();
        assert_eq!(rejection(read_forged(&tar)), EntryRejection::PathCharacter);
    }
}

#[test]
fn reader_rejects_unsorted_entries() {
    let tar = Forged::new().file(b"b", b"1").file(b"a", b"2").end();
    assert_eq!(rejection(read_forged(&tar)), EntryRejection::Unsorted);
}

// ------------------------------------------------------------- entry types

fn typed(typeflag: u8) -> Result<package::VerifiedArchive, ArchiveError> {
    let block = header(b"link", 0, typeflag);
    read_forged(&Forged::new().block(&block, b"").end())
}

#[test]
fn reader_rejects_symlinks() {
    assert_eq!(rejection(typed(b'2')), EntryRejection::Symlink);
}

#[test]
fn reader_rejects_hardlinks() {
    assert_eq!(rejection(typed(b'1')), EntryRejection::Hardlink);
}

#[test]
fn reader_rejects_devices_and_fifos() {
    for flag in *b"346" {
        assert_eq!(rejection(typed(flag)), EntryRejection::SpecialFile);
    }
}

#[test]
fn reader_rejects_directories() {
    assert_eq!(rejection(typed(b'5')), EntryRejection::Directory);
}

#[test]
fn reader_rejects_pax_and_gnu_extension_headers() {
    for flag in *b"xgLKS" {
        assert_eq!(rejection(typed(flag)), EntryRejection::ExtensionHeader);
    }
}

#[test]
fn reader_rejects_unknown_type_flags() {
    for flag in [b'7', b'Z', 0, b'9'] {
        assert_eq!(rejection(typed(flag)), EntryRejection::UnsupportedType);
    }
}

// ------------------------------------------------------ canonical metadata

#[test]
fn reader_rejects_non_canonical_mtime() {
    let result = mutated(|b| octal(&mut b[MTIME..MTIME + 12], 1));
    assert_eq!(rejection(result), EntryRejection::NonCanonicalHeader);
}

#[test]
fn reader_rejects_non_canonical_uid_and_gid() {
    let uid = mutated(|b| octal(&mut b[UID..UID + 8], 1000));
    assert_eq!(rejection(uid), EntryRejection::NonCanonicalHeader);
    let gid = mutated(|b| octal(&mut b[GID..GID + 8], 1000));
    assert_eq!(rejection(gid), EntryRejection::NonCanonicalHeader);
}

#[test]
fn reader_rejects_non_canonical_modes() {
    for mode in [0o600, 0o664, 0o777, 0o4755] {
        let result = mutated(|b| octal(&mut b[MODE..MODE + 8], mode));
        assert_eq!(rejection(result), EntryRejection::NonCanonicalHeader);
    }
}

#[test]
fn reader_rejects_owner_names_prefix_and_linkname() {
    let uname = mutated(|b| b[UNAME..UNAME + 4].copy_from_slice(b"root"));
    assert_eq!(rejection(uname), EntryRejection::NonCanonicalHeader);
    let gname = mutated(|b| b[GNAME..GNAME + 4].copy_from_slice(b"root"));
    assert_eq!(rejection(gname), EntryRejection::NonCanonicalHeader);
    let prefix = mutated(|b| b[PREFIX..PREFIX + 3].copy_from_slice(b"dir"));
    assert_eq!(rejection(prefix), EntryRejection::NonCanonicalHeader);
    let link = mutated(|b| b[LINKNAME..LINKNAME + 3].copy_from_slice(b"tgt"));
    assert_eq!(rejection(link), EntryRejection::NonCanonicalHeader);
}

#[test]
fn reader_rejects_bytes_after_the_name_terminator() {
    let result = mutated(|b| b[NAME_END - 1] = b'x');
    assert_eq!(rejection(result), EntryRejection::NonCanonicalHeader);
}

#[test]
fn reader_rejects_a_wrong_header_checksum() {
    let mut block = header(b"runtime.toml", 3, b'0');
    block[CHECKSUM..CHECKSUM + 7].copy_from_slice(b"0000001");
    let tar = Forged::new().block(&block, b"abc").end();
    assert_eq!(
        rejection(read_forged(&tar)),
        EntryRejection::NonCanonicalHeader
    );
}

#[test]
fn reader_rejects_malformed_size_fields() {
    let result = mutated(|b| b[124..136].copy_from_slice(b"0000000008\0\0"));
    assert_eq!(rejection(result), EntryRejection::NonCanonicalHeader);
    let result = mutated(|b| b[124..136].copy_from_slice(b"          3 "));
    assert_eq!(rejection(result), EntryRejection::NonCanonicalHeader);
}

#[test]
fn reader_rejects_a_missing_magic() {
    let result = mutated(|b| b[257..263].fill(0));
    assert_eq!(rejection(result), EntryRejection::NonCanonicalHeader);
}

#[test]
fn reader_rejects_non_zero_padding() {
    let mut tar = Forged::new().file(b"a", b"abc").end();
    tar[BLOCK + 3] = 1;
    assert_eq!(
        rejection(read_forged(&tar)),
        EntryRejection::NonCanonicalPadding
    );
}

// ------------------------------------------------------- stream structure

#[test]
fn reader_rejects_trailing_tar_data() {
    let mut tar = Forged::new().file(b"a", b"x").end();
    tar.extend_from_slice(&[0_u8; BLOCK]);
    assert_eq!(read_forged(&tar).unwrap_err(), ArchiveError::TrailingData);

    let mut tar = Forged::new().file(b"a", b"x").end();
    tar.extend_from_slice(b"garbage");
    assert_eq!(read_forged(&tar).unwrap_err(), ArchiveError::TrailingData);

    let mut tar = Forged::new().file(b"a", b"x").end();
    let at = tar.len() - 1;
    tar[at] = 1;
    assert_eq!(read_forged(&tar).unwrap_err(), ArchiveError::TrailingData);
}

#[test]
fn reader_rejects_missing_or_short_end_marker() {
    let mut tar = Forged::new().file(b"a", b"x").end();
    tar.truncate(tar.len() - 2 * BLOCK);
    assert_eq!(read_forged(&tar).unwrap_err(), ArchiveError::Truncated);
    tar.extend_from_slice(&[0_u8; BLOCK]);
    assert_eq!(read_forged(&tar).unwrap_err(), ArchiveError::Truncated);
}

#[test]
fn reader_rejects_truncated_file_data() {
    let mut tar = Forged::new().file(b"a", &[1_u8; 600]).end();
    tar.truncate(BLOCK + 100);
    assert_eq!(read_forged(&tar).unwrap_err(), ArchiveError::Truncated);
}

#[test]
fn reader_rejects_trailing_bytes_after_the_zstd_frame() {
    let mut bytes = build_archive(&sample(), &Limits::DEFAULT).expect("build");
    bytes.push(0);
    assert_eq!(
        read_archive(&bytes, &Limits::DEFAULT).unwrap_err(),
        ArchiveError::TrailingCompressedData
    );

    // A second complete frame is also trailing data.
    let mut bytes = build_archive(&sample(), &Limits::DEFAULT).expect("build");
    bytes.extend_from_slice(&build_archive(&sample(), &Limits::DEFAULT).expect("build"));
    assert_eq!(
        read_archive(&bytes, &Limits::DEFAULT).unwrap_err(),
        ArchiveError::TrailingCompressedData
    );
}

#[test]
fn reader_rejects_a_corrupted_content_checksum() {
    let mut bytes = build_archive(&sample(), &Limits::DEFAULT).expect("build");
    let at = bytes.len() - 1;
    bytes[at] ^= 0xff;
    assert_eq!(
        read_archive(&bytes, &Limits::DEFAULT).unwrap_err(),
        ArchiveError::ZstdChecksum
    );
}

#[test]
fn reader_rejects_frames_without_a_content_checksum() {
    // Magic, single-segment descriptor without checksum, content size 1, one
    // final raw block of one byte.
    let frame = [0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x01, 0x09, 0x00, 0x00, 0x00];
    assert_eq!(
        read_archive(&frame, &lenient()).unwrap_err(),
        ArchiveError::ZstdChecksum
    );
}

#[test]
fn reader_rejects_non_zstd_and_skippable_frames() {
    assert_eq!(
        read_archive(b"definitely not zstd", &Limits::DEFAULT).unwrap_err(),
        ArchiveError::ZstdHeader
    );
    assert_eq!(
        read_archive(&[], &Limits::DEFAULT).unwrap_err(),
        ArchiveError::ZstdHeader
    );
    let skippable = [0x50, 0x2a, 0x4d, 0x18, 0, 0, 0, 0];
    assert_eq!(
        read_archive(&skippable, &Limits::DEFAULT).unwrap_err(),
        ArchiveError::ZstdHeader
    );
}

#[test]
fn reader_rejects_truncated_zstd_frames() {
    let bytes = build_archive(&sample(), &Limits::DEFAULT).expect("build");
    let error = read_archive(&bytes[..bytes.len() / 2], &Limits::DEFAULT).unwrap_err();
    assert!(matches!(
        error,
        ArchiveError::ZstdCorrupt | ArchiveError::ZstdChecksum
    ));
}

#[test]
fn reader_rejects_oversized_zstd_windows_before_allocating() {
    // Non-single-segment frame whose window descriptor declares 2^40 bytes.
    let window_descriptor = 30_u8 << 3;
    let frame = [
        0x28,
        0xb5,
        0x2f,
        0xfd,
        0x04,
        window_descriptor,
        0x01,
        0x00,
        0x00,
    ];
    assert_eq!(
        read_archive(&frame, &Limits::DEFAULT).unwrap_err(),
        ArchiveError::ZstdWindow {
            limit: Limits::DEFAULT.max_window_bytes
        }
    );
}

// ------------------------------------------------------------------- limits

#[test]
fn compressed_size_limit_boundary() {
    let bytes = build_archive(&sample(), &Limits::DEFAULT).expect("build");
    let len = bytes.len() as u64;
    let at = Limits {
        max_compressed_bytes: len,
        ..Limits::DEFAULT
    };
    read_archive(&bytes, &at).expect("exactly at the limit");
    let below = Limits {
        max_compressed_bytes: len - 1,
        ..Limits::DEFAULT
    };
    assert_eq!(
        read_archive(&bytes, &below).unwrap_err(),
        ArchiveError::CompressedTooLarge { limit: len - 1 }
    );
}

#[test]
fn expanded_size_limit_boundary() {
    let entries = sample();
    let tar = tar_len(&entries);
    let bytes = build_archive(&entries, &Limits::DEFAULT).expect("build");
    let at = Limits {
        max_expanded_bytes: tar,
        ..Limits::DEFAULT
    };
    read_archive(&bytes, &at).expect("exactly at the limit");
    let below = Limits {
        max_expanded_bytes: tar - 1,
        ..Limits::DEFAULT
    };
    assert_eq!(
        read_archive(&bytes, &below).unwrap_err(),
        ArchiveError::ExpandedTooLarge { limit: tar - 1 }
    );
    assert_eq!(
        build_archive(&entries, &below).unwrap_err(),
        ArchiveError::ExpandedTooLarge { limit: tar - 1 }
    );
}

#[test]
fn expansion_ratio_limit_boundary() {
    let entries = sample();
    let tar = tar_len(&entries);
    let bytes = build_archive(&entries, &Limits::DEFAULT).expect("build");
    let compressed = bytes.len() as u64;
    let ratio = tar.div_ceil(compressed);
    assert!((ratio - 1) * compressed < tar, "boundary needs a real gap");
    let at = Limits {
        max_expansion_ratio: ratio,
        ..Limits::DEFAULT
    };
    read_archive(&bytes, &at).expect("at the ratio");
    let below = Limits {
        max_expansion_ratio: ratio - 1,
        ..Limits::DEFAULT
    };
    assert_eq!(
        read_archive(&bytes, &below).unwrap_err(),
        ArchiveError::ExpansionRatio { limit: ratio - 1 }
    );
}

#[test]
fn decompression_bomb_is_rejected_by_the_default_limits() {
    // One maximum-size file of zeros compresses to a few hundred bytes.
    let zeros = vec![0_u8; usize::try_from(Limits::DEFAULT.max_file_bytes).expect("fits")];
    let tar = Forged::new().file(b"zeros", &zeros).end();
    let bomb = zstd(&tar);
    assert!(
        (bomb.len() as u64) * Limits::DEFAULT.max_expansion_ratio < tar.len() as u64,
        "fixture must exceed the ratio"
    );
    assert_eq!(
        read_archive(&bomb, &Limits::DEFAULT).unwrap_err(),
        ArchiveError::ExpansionRatio {
            limit: Limits::DEFAULT.max_expansion_ratio
        }
    );
}

#[test]
fn bomb_beyond_the_expanded_ceiling_stops_at_the_ceiling() {
    let ceiling = 1024 * 1024;
    let limits = Limits {
        max_expanded_bytes: ceiling,
        max_expansion_ratio: u64::MAX,
        max_file_bytes: u64::MAX,
        ..Limits::DEFAULT
    };
    let tar = Forged::new()
        .file(b"zeros", &vec![0_u8; 4 * 1024 * 1024])
        .end();
    assert_eq!(
        read_archive(&zstd(&tar), &limits).unwrap_err(),
        ArchiveError::ExpandedTooLarge { limit: ceiling }
    );
}

#[test]
fn file_count_limit_boundary() {
    let entries: Vec<ArchiveEntry> = (0..4).map(|n| entry(&format!("f{n}"), b"x")).collect();
    let at = Limits {
        max_files: 4,
        ..Limits::DEFAULT
    };
    let bytes = build_archive(&entries, &at).expect("exactly at the limit");
    read_archive(&bytes, &at).expect("read at the limit");
    let below = Limits {
        max_files: 3,
        ..Limits::DEFAULT
    };
    assert_eq!(
        read_archive(&bytes, &below).unwrap_err(),
        ArchiveError::TooManyFiles { limit: 3 }
    );
    assert_eq!(
        build_archive(&entries, &below).unwrap_err(),
        ArchiveError::TooManyFiles { limit: 3 }
    );
}

#[test]
fn path_length_limit_boundary() {
    let at = "a".repeat(100);
    let over = "a".repeat(101);
    let bytes = build_archive(&[entry(&at, b"x")], &Limits::DEFAULT).expect("100 bytes fit");
    read_archive(&bytes, &Limits::DEFAULT).expect("read");
    assert_eq!(
        build_archive(&[entry(&over, b"x")], &Limits::DEFAULT).unwrap_err(),
        ArchiveError::Entry {
            index: 0,
            reason: EntryRejection::PathTooLong
        }
    );

    let short = Limits {
        max_path_bytes: 10,
        ..Limits::DEFAULT
    };
    build_archive(&[entry(&"a".repeat(10), b"x")], &short).expect("10 bytes fit");
    assert_eq!(
        build_archive(&[entry(&"a".repeat(11), b"x")], &short).unwrap_err(),
        ArchiveError::Entry {
            index: 0,
            reason: EntryRejection::PathTooLong
        }
    );
}

#[test]
fn path_limit_is_capped_at_the_ustar_name_field() {
    // A 101-byte name cannot be stored; a longer limit is capped at 100.
    let long = Limits {
        max_path_bytes: 1000,
        ..Limits::DEFAULT
    };
    assert_eq!(
        build_archive(&[entry(&"a".repeat(101), b"x")], &long).unwrap_err(),
        ArchiveError::Entry {
            index: 0,
            reason: EntryRejection::PathTooLong
        }
    );
}

#[test]
fn per_file_size_limit_boundary() {
    let at = Limits {
        max_file_bytes: 1024,
        ..Limits::DEFAULT
    };
    let ok = entry("a", &[1_u8; 1024]);
    let bytes = build_archive(&[ok], &at).expect("exactly at the limit");
    read_archive(&bytes, &at).expect("read");
    assert_eq!(
        build_archive(&[entry("a", &[1_u8; 1025])], &at).unwrap_err(),
        ArchiveError::Entry {
            index: 0,
            reason: EntryRejection::FileTooLarge
        }
    );
    // The reader enforces it independently of the writer.
    let tar = Forged::new().file(b"a", &[1_u8; 1025]).end();
    assert_eq!(
        rejection(read_archive(
            &zstd(&tar),
            &Limits {
                max_expansion_ratio: u64::MAX,
                ..at
            }
        )),
        EntryRejection::FileTooLarge
    );
}

// ------------------------------------------------------------ builder rules

#[test]
fn builder_rejects_the_same_paths_as_the_reader() {
    let cases = [
        ("../a", EntryRejection::PathDotSegment),
        ("/a", EntryRejection::PathAbsolute),
        ("./a", EntryRejection::PathDotSegment),
        ("a//b", EntryRejection::PathEmptySegment),
        ("", EntryRejection::PathEmpty),
        ("a b", EntryRejection::PathCharacter),
        ("caf\u{e9}", EntryRejection::PathCharacter),
        ("a\u{1}", EntryRejection::PathCharacter),
    ];
    for (path, reason) in cases {
        assert_eq!(
            build_archive(&[entry(path, b"x")], &Limits::DEFAULT).unwrap_err(),
            ArchiveError::Entry { index: 0, reason },
            "path {path:?}"
        );
    }
}

#[test]
fn builder_rejects_duplicates_case_collisions_and_conflicts() {
    let duplicate = [entry("a", b"1"), entry("a", b"2")];
    assert!(matches!(
        build_archive(&duplicate, &Limits::DEFAULT).unwrap_err(),
        ArchiveError::Entry {
            reason: EntryRejection::Duplicate,
            ..
        }
    ));
    let collision = [entry("A", b"1"), entry("a", b"2")];
    assert!(matches!(
        build_archive(&collision, &Limits::DEFAULT).unwrap_err(),
        ArchiveError::Entry {
            reason: EntryRejection::CaseCollision,
            ..
        }
    ));
    let conflict = [entry("a", b"1"), entry("a/b", b"2")];
    assert!(matches!(
        build_archive(&conflict, &Limits::DEFAULT).unwrap_err(),
        ArchiveError::Entry {
            reason: EntryRejection::FileDirectoryConflict,
            ..
        }
    ));
}

#[test]
fn error_messages_never_echo_entry_names() {
    let secret = "SECRET_NAME_\u{1b}[31m";
    let error = build_archive(&[entry(secret, b"x")], &Limits::DEFAULT).unwrap_err();
    let text = error.to_string();
    assert!(!text.contains("SECRET"), "{text}");
    assert!(!text.contains('\u{1b}'), "{text}");

    let tar = Forged::new().file(b"../SECRET", b"x").end();
    let text = read_forged(&tar).unwrap_err().to_string();
    assert!(!text.contains("SECRET"), "{text}");
}

// ------------------------------------------- one identity per archive content

#[test]
fn reader_rejects_a_valid_zstd_frame_that_is_not_the_canonical_encoding() {
    let canonical = build_archive(&sample(), &Limits::DEFAULT).expect("build");
    let tar = ruzstd_decode(&canonical);
    let alternate = compress_to_vec(tar.as_slice(), CompressionLevel::Uncompressed);
    assert_ne!(
        alternate, canonical,
        "fixture must differ from the canonical frame"
    );
    assert_eq!(
        read_archive(&alternate, &lenient()).unwrap_err(),
        ArchiveError::NonCanonicalCompression
    );
}

// ------------------------------------------------- expanded size before copy

#[test]
fn builder_applies_the_expanded_limit_before_copying_entries() {
    // Each entry is valid on its own; only the aggregate exceeds the limit.
    let entries: Vec<ArchiveEntry> = (0..8)
        .map(|n| entry(&format!("f{n}"), &[1_u8; 4096]))
        .collect();
    let limits = Limits {
        max_expanded_bytes: tar_len(&entries) - 1,
        ..Limits::DEFAULT
    };
    assert_eq!(
        build_archive(&entries, &limits).unwrap_err(),
        ArchiveError::ExpandedTooLarge {
            limit: limits.max_expanded_bytes
        }
    );
    let at = Limits {
        max_expanded_bytes: tar_len(&entries),
        max_expansion_ratio: u64::MAX,
        ..Limits::DEFAULT
    };
    build_archive(&entries, &at).expect("exactly at the aggregate limit");
}
