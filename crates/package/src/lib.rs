//! Canonical runtime package archive format, builder and strict reader.
//!
//! A runtime package ships as one deterministic `tar.zst` archive. This crate
//! owns the byte-level contract:
//!
//! - [`build_archive`] turns a set of files into the canonical archive bytes.
//!   The output depends only on the file set, never on input order, host
//!   clocks, users or filesystem metadata.
//! - [`read_archive`] and [`read_archive_with_digest`] parse archive bytes
//!   strictly into memory. Anything that is not byte-for-byte the canonical
//!   form is rejected with a typed [`ArchiveError`] that never echoes archive
//!   content.
//! - [`PackageDigest`] (SHA-256 of the archive bytes) authenticates an archive.
//!
//! Extraction to disk, package manifests and signatures are separate concerns
//! built on top of [`VerifiedArchive`]. The format is described in
//! `docs/knowledge/concepts/runtime-package-archive.md`.

// Rust guideline compliant 2026-10-04

#![forbid(unsafe_code)]

mod archive;
mod canonical;
mod compression;
mod error;
mod limits;

pub use archive::{
    build_archive, read_archive, read_archive_with_digest, ArchiveEntry, VerifiedArchive,
};
pub use error::{ArchiveError, EntryRejection};
pub use limits::{
    Limits, MAX_COMPRESSED_BYTES, MAX_EXPANDED_BYTES, MAX_EXPANSION_RATIO, MAX_FILES,
    MAX_FILE_BYTES, MAX_PATH_BYTES, MAX_WINDOW_BYTES,
};
pub use protocol::PackageDigest;
