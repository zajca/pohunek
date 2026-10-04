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
//! - [`verify_catalog`] authenticates the signed runtime catalog that
//!   authorizes official packages by archive digest; [`LocalTrust`] is the
//!   separate explicit-digest trust for third-party archives.
//!
//! Extraction to disk and package manifests are separate concerns built on
//! top of [`VerifiedArchive`]. The formats are described in
//! `docs/knowledge/concepts/runtime-package-archive.md` and
//! `docs/knowledge/concepts/runtime-catalog.md`.

// Rust guideline compliant 2026-10-04

#![forbid(unsafe_code)]

mod archive;
mod canonical;
mod canonical_json;
mod catalog;
mod compression;
mod error;
mod limits;

pub use archive::{
    build_archive, read_archive, read_archive_with_digest, ArchiveEntry, VerifiedArchive,
};
pub use catalog::{
    catalog_signing_message, key_record_signing_message, verify_catalog, Authorization, Catalog,
    CatalogEntry, CatalogEntryRejection, CatalogEnvelope, CatalogError, CatalogLimit,
    CatalogSignature, KeyId, LocalAuthorization, LocalTrust, LocalTrustError, RootKey,
    SignedKeyRecord, TrustAnchor, TrustAnchorError, VerifiedCatalog, VerifiedEntry,
    CATALOG_SCHEMA_VERSION, MAX_CATALOG_BYTES, MAX_CATALOG_ENTRIES, MAX_CORE_RANGE_BYTES,
    MAX_KEY_RECORDS, MAX_PLATFORMS_PER_ENTRY, MAX_PLATFORM_BYTES, MAX_REVOKED_DIGESTS,
    MAX_REVOKED_KEYS, MAX_SIGNATURES, MAX_TRUST_ROOTS,
};
pub use error::{ArchiveError, EntryRejection};
pub use limits::{
    Limits, MAX_COMPRESSED_BYTES, MAX_EXPANDED_BYTES, MAX_EXPANSION_RATIO, MAX_FILES,
    MAX_FILE_BYTES, MAX_PATH_BYTES, MAX_WINDOW_BYTES,
};
pub use protocol::PackageDigest;
