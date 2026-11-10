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
//!   [`archive_digest`] computes it without decompressing, so a caller can
//!   authorize the digest before [`read_archive_with_digest`] decodes anything.
//! - [`parse_anchor`] reads the trust anchor file of a release bundle and
//!   [`sign_catalog`] signs a catalog for release tooling.
//! - [`verify_catalog`] authenticates the signed runtime catalog that
//!   authorizes official packages by archive digest; [`LocalTrust`] is the
//!   separate explicit-digest trust for third-party archives.
//! - [`verify_index`] authenticates the signed plugin index that authorizes
//!   plugin packages by archive digest; [`VerifiedIndex::authorize`] applies
//!   the per-entry install-time checks. [`parse_index_channel`] reads the
//!   channel file that names the index endorser root of the trust anchor.
//!
//! On Unix the crate also stores verified archives:
//!
//! - [`install`] extracts a [`VerifiedArchive`] into an owner-private,
//!   content-addressed package root through directory descriptors.
//! - [`verify`] re-verifies a root against its per-file [`manifest`].
//! - [`registry`] keeps the transactional record of installed packages and, in
//!   the same plugin root, the monotonic catalog state ([`catalog_state`]) the
//!   catalog verifier needs from its caller: the sequence high-water mark and
//!   the accumulated revoked key ids, and the same for the plugin index
//!   verifier (`Registry::index_state`, `Registry::record_index`).
//! - [`directory`] builds the canonical archive of a developer directory, the
//!   input of `plugin link` and `package build`.
//!
//! - [`version_probe`] is the one grammar of a descriptor's `version_probe`
//!   declaration and of the version output it reads, shared by the daemon's
//!   launch gate and the CLI's Hermes integration gate.
//!
//! Signatures are covered by the signed runtime catalog; the runtime manifest
//! inside a package is a separate concern. The formats are described in
//! `docs/knowledge/concepts/runtime-package-archive.md` and
//! `docs/knowledge/concepts/runtime-catalog.md`.

// Rust guideline compliant 2026-10-13

#![forbid(unsafe_code)]

mod anchor;
mod archive;
mod canonical;
mod canonical_json;
mod catalog;
mod compression;
mod error;
mod index_channel;
mod limits;
mod platform;
mod plugin_index;
mod release;
mod sign;
pub mod version_probe;

// Extraction, verification and the registry need descriptor-relative
// filesystem primitives.
#[cfg(unix)]
pub mod catalog_state;
#[cfg(unix)]
pub mod directory;
#[cfg(unix)]
mod hash;
#[cfg(unix)]
mod index_state;
#[cfg(unix)]
pub mod install;
#[cfg(unix)]
mod layout;
#[cfg(unix)]
pub mod manifest;
#[cfg(unix)]
pub mod registry;
#[cfg(unix)]
pub mod revoked_digests;
#[cfg(unix)]
pub mod verify;

pub use anchor::{
    parse_anchor, AnchorFile, AnchorFileError, ANCHOR_FILE_NAME, ANCHOR_SCHEMA_VERSION,
    MAX_ANCHOR_BYTES,
};
pub use archive::{
    archive_digest, build_archive, read_archive, read_archive_with_digest, ArchiveEntry,
    VerifiedArchive,
};
pub use catalog::{
    catalog_signing_message, check_catalog, key_record_signing_message, parse_core_range,
    verify_catalog, Authorization, Catalog, CatalogEntry, CatalogEntryRejection, CatalogEnvelope,
    CatalogError, CatalogLimit, CatalogSignature, KeyId, LocalAuthorization, LocalTrust,
    LocalTrustError, RootKey, SignedKeyRecord, TrustAnchor, TrustAnchorError, VerifiedCatalog,
    VerifiedEntry, CATALOG_SCHEMA_VERSION, MAX_CATALOG_BYTES, MAX_CATALOG_ENTRIES,
    MAX_CORE_RANGE_BYTES, MAX_KEY_RECORDS, MAX_PLATFORMS_PER_ENTRY, MAX_PLATFORM_BYTES,
    MAX_REVOKED_DIGESTS, MAX_REVOKED_KEYS, MAX_SIGNATURES, MAX_TRUST_ROOTS,
};
pub use error::{ArchiveError, EntryRejection};
pub use index_channel::{
    parse_index_channel, IndexChannel, IndexChannelError, INDEX_CHANNEL_FILE_NAME,
    INDEX_CHANNEL_SCHEMA_VERSION, MAX_INDEX_CHANNEL_BYTES,
};
pub use limits::{
    Limits, MAX_COMPRESSED_BYTES, MAX_EXPANDED_BYTES, MAX_EXPANSION_RATIO, MAX_FILES,
    MAX_FILE_BYTES, MAX_PATH_BYTES, MAX_WINDOW_BYTES,
};
pub use platform::host_platform;
pub use plugin_index::{
    check_index_body, index_document_signing_message, index_key_record_signing_message,
    index_signing_message, parse_index_envelope, revocation_signing_message, verify_index,
    verify_key_record_document, verify_revocation_document, IndexBody, IndexEntry,
    IndexEntryRefusal, IndexEntryRejection, IndexEnvelope, IndexError, IndexKeyRecord, IndexLimit,
    IndexVerifierState, RevocationBody, SignedRevocations, VerifiedIndex,
    INDEX_REFUSED_RUNTIME_IDS, INDEX_SCHEMA_VERSION, MAX_ENTRY_URL_BYTES, MAX_INDEX_BYTES,
    MAX_INDEX_ENTRIES, MAX_INDEX_KEY_RECORDS, MAX_INDEX_KEY_WINDOW_SECONDS,
    MAX_INDEX_LIFETIME_SECONDS, MAX_INDEX_SEQUENCE_SKEW_SECONDS, MAX_INDEX_SIGNATURES,
    OFFICIAL_INDEX_FILE_NAME, OFFICIAL_NAMESPACE, OFFICIAL_RUNTIME_PACKAGE_PREFIX, OFFICIAL_SCOPE,
};
pub use protocol::PackageDigest;
pub use release::{
    Attestation, BinarySet, Release, ReleaseRejection, Sha256Digest, VerifiedRelease,
    MAX_ATTESTATIONS_PER_ENTRY, MAX_BINARY_SETS,
};
pub use sign::{catalog_document_bytes, sign_catalog};
