//! Signed runtime catalog: format, trust model and verification.
//!
//! Official runtime packages are authorized by a catalog that binds each
//! package archive digest to a package id, runtime id, version, platforms and
//! a core version range. The catalog is signed with Ed25519 over its canonical
//! JSON bytes; individual packages carry no signature. A third-party archive
//! is trusted only through [`LocalTrust`], an explicit owner-supplied digest
//! that can never yield [`Authorization::Official`].
//!
//! Trust flows from a [`TrustAnchor`] the caller supplies (root public keys
//! with validity windows plus a revoked key list). A catalog may carry a
//! `key_chain` of [`SignedKeyRecord`]s: each record names a new key and is
//! signed by an earlier key in the chain or by a root, which is how signing
//! keys rotate without shipping a new anchor. [`verify_catalog`] checks the
//! signature, key windows, revocation, expiry and anti-rollback sequence, and
//! returns a [`VerifiedCatalog`]. Every failure is a typed [`CatalogError`]
//! that never carries catalog content.
//!
//! Persistence of the sequence high-water mark and of accumulated revoked key
//! ids belongs to the caller; this module is pure and clock-free (`now` is an
//! explicit Unix timestamp in seconds).
//!
//! The format is described in `docs/knowledge/concepts/runtime-catalog.md`.

// Rust guideline compliant 2026-10-04

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display, Formatter};

use ed25519_dalek::{Signature, VerifyingKey};
use protocol::{PackageDigest, PackageId, PackageVersion, RuntimeId};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::canonical_json::{canonical_bytes, parse_strict, JsonError};

/// Catalog schema version this build reads and writes.
pub const CATALOG_SCHEMA_VERSION: u64 = 1;

/// Largest accepted catalog document: 1 MiB.
///
/// Checked before parsing. A catalog lists a handful of official packages
/// (three runtimes times a few platforms and versions); this ceiling leaves
/// room for growth while bounding what is parsed before authentication.
pub const MAX_CATALOG_BYTES: usize = 1024 * 1024;

/// Largest accepted number of catalog entries: 256.
///
/// Three official runtimes with a few versions and platforms each stay far
/// below this; it bounds per-entry validation work on unauthenticated input.
/// Lowering it below the real release inventory makes valid catalogs fail.
pub const MAX_CATALOG_ENTRIES: usize = 256;

/// Largest accepted number of platforms in one entry: 16.
///
/// Covers the release target matrix with headroom; bounds the per-entry
/// uniqueness check.
pub const MAX_PLATFORMS_PER_ENTRY: usize = 16;

/// Largest accepted platform name: 64 bytes (a target triple is under 40).
pub const MAX_PLATFORM_BYTES: usize = 64;

/// Largest accepted core version range string: 128 bytes.
///
/// A range is a few comparators such as `>=0.33.0, <0.40.0`; the ceiling
/// bounds what the semver parser sees before authentication.
pub const MAX_CORE_RANGE_BYTES: usize = 128;

/// Largest accepted number of key chain records in one catalog: 16.
///
/// Rotation adds one record per step; a long chain means the anchor should be
/// refreshed instead.
pub const MAX_KEY_RECORDS: usize = 16;

/// Largest accepted number of detached signatures on one catalog: 8.
///
/// A release uses one signature, or two during a key rotation overlap; each
/// extra signature costs one Ed25519 verification.
pub const MAX_SIGNATURES: usize = 8;

/// Largest accepted number of revoked key ids in a catalog or anchor: 64.
///
/// Keys are rotated rarely, so the list stays short for the life of the
/// project; the cap bounds the set built on every verification.
pub const MAX_REVOKED_KEYS: usize = 64;

/// Largest accepted number of revoked package digests in a catalog: 1024.
///
/// Digest revocations accumulate across releases because each catalog carries
/// the full list; this leaves room for years of withdrawn packages while
/// keeping the 1 MiB document limit meaningful (a digest entry is about 80
/// bytes).
pub const MAX_REVOKED_DIGESTS: usize = 1024;

/// Largest accepted number of trust anchor roots: 8.
///
/// An anchor holds the active release root plus a few overlapping successors
/// during rotation. A larger set widens the set of keys able to start a chain.
pub const MAX_TRUST_ROOTS: usize = 8;

/// Domain separator prefixed to the canonical catalog bytes before signing, so
/// a catalog signature can never be replayed as another kind of signature.
const CATALOG_DOMAIN: &[u8] = b"pohunek-runtime-catalog-v1\n";

/// Domain separator prefixed to the canonical key record body before signing.
const KEY_RECORD_DOMAIN: &[u8] = b"pohunek-catalog-key-record-v1\n";

/// Length of an Ed25519 public key in bytes.
const PUBLIC_KEY_BYTES: usize = 32;

/// Length of an Ed25519 signature in bytes.
const SIGNATURE_BYTES: usize = 64;

/// Length of a SHA-256 digest in bytes, the size of a [`KeyId`].
const KEY_ID_BYTES: usize = 32;

/// Runtime ids no package may claim without catalog authorization.
const OFFICIAL_RUNTIMES: [&str; 3] = [RuntimeId::CODEX, RuntimeId::CLAUDE, RuntimeId::HERMES];

/// Lowercase hexadecimal digits.
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

fn encode_hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(HEX_DIGITS[usize::from(byte & 0x0f)]));
    }
    text
}

/// Decodes exactly `N` bytes of lowercase hexadecimal.
fn decode_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let bytes = text.as_bytes();
    if bytes.len() != N * 2 {
        return None;
    }
    let nibble = |digit: u8| match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    };
    let mut out = [0_u8; N];
    for (index, slot) in out.iter_mut().enumerate() {
        let high = nibble(*bytes.get(index * 2)?)?;
        let low = nibble(*bytes.get(index * 2 + 1)?)?;
        *slot = (high << 4) | low;
    }
    Some(out)
}

/// Identifier of an Ed25519 verifying key: the lowercase hex SHA-256 of the
/// 32 raw public key bytes. Derived, never chosen, so a record cannot claim an
/// id that does not match its key.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct KeyId(String);

impl KeyId {
    /// Derives the id of a verifying key.
    #[must_use]
    pub fn derive(key: &VerifyingKey) -> Self {
        Self(encode_hex(&Sha256::digest(key.as_bytes())))
    }

    /// Parses the 64-character lowercase hex form.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::Schema`] when `text` is not 64 lowercase hex
    /// characters.
    pub fn parse(text: &str) -> Result<Self, CatalogError> {
        decode_hex::<KEY_ID_BYTES>(text)
            .map(|_bytes| Self(text.to_owned()))
            .ok_or(CatalogError::Schema)
    }

    /// The 64-character lowercase hex form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for KeyId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for KeyId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for KeyId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(|_cause| serde::de::Error::custom("invalid key id"))
    }
}

/// One official package version authorized by the catalog.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogEntry {
    /// Package identity.
    pub package_id: PackageId,
    /// Runtime the package provides.
    pub runtime_id: RuntimeId,
    /// Package version.
    pub version: PackageVersion,
    /// Digest of the package archive bytes.
    pub digest: PackageDigest,
    /// Platforms the package supports, as lowercase `[a-z0-9._-]` names.
    pub platforms: Vec<String>,
    /// Semver range of core versions the package supports.
    pub core: String,
}

/// The signed catalog body.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    /// Must equal [`CATALOG_SCHEMA_VERSION`].
    pub schema_version: u64,
    /// Strictly increasing publication counter used for anti-rollback.
    pub sequence: u64,
    /// Unix seconds from which the catalog must no longer be accepted.
    pub expires_at: u64,
    /// Keys that must not sign or endorse, in addition to the anchor's list.
    pub revoked_key_ids: Vec<KeyId>,
    /// Package digests that are never authorized.
    pub revoked_digests: Vec<PackageDigest>,
    /// Authorized package versions.
    pub entries: Vec<CatalogEntry>,
}

/// A signing key introduced into the chain and endorsed by an earlier key.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignedKeyRecord {
    /// Id derived from `public_key`.
    pub key_id: KeyId,
    /// 64 lowercase hex characters of the raw Ed25519 public key.
    pub public_key: String,
    /// First Unix second the key may sign.
    pub not_before: u64,
    /// Unix second from which the key may no longer sign.
    pub not_after: u64,
    /// Id of the root or earlier chain key that endorses this key.
    pub endorsed_by: KeyId,
    /// 128 lowercase hex characters: the endorser's signature over
    /// [`key_record_signing_message`] of this record.
    pub signature: String,
}

/// One detached signature over the catalog.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogSignature {
    /// Key that produced the signature.
    pub key_id: KeyId,
    /// 128 lowercase hex characters over [`catalog_signing_message`].
    pub signature: String,
}

/// The complete `runtime-catalog.json` document.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogEnvelope {
    /// Must equal [`CATALOG_SCHEMA_VERSION`].
    pub schema_version: u64,
    /// The signed body.
    pub catalog: Catalog,
    /// Rotation records, each endorsed by a root or an earlier record.
    pub key_chain: Vec<SignedKeyRecord>,
    /// Detached signatures over the catalog body.
    pub signatures: Vec<CatalogSignature>,
}

/// Which bounded collection exceeded its limit.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CatalogLimit {
    /// Too many catalog entries.
    #[error("entries")]
    Entries,
    /// Too many platforms in one entry.
    #[error("platforms")]
    Platforms,
    /// Too many key chain records.
    #[error("key records")]
    KeyRecords,
    /// Too many signatures.
    #[error("signatures")]
    Signatures,
    /// Too many revoked key ids.
    #[error("revoked keys")]
    RevokedKeys,
    /// Too many revoked digests.
    #[error("revoked digests")]
    RevokedDigests,
}

/// Why one catalog entry was rejected.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CatalogEntryRejection {
    /// The runtime id is `shell`, which no package may claim.
    #[error("runtime id `shell` is reserved")]
    ReservedShell,
    /// The platform list is empty, has a bad name, or repeats a name.
    #[error("platform list is invalid")]
    Platforms,
    /// The core range is empty, too long, unparsable or matches every version.
    #[error("core version range is invalid")]
    CoreRange,
    /// The same package id and version appear twice.
    #[error("package version is listed twice")]
    DuplicateVersion,
    /// The same archive digest appears twice.
    #[error("archive digest is listed twice")]
    DuplicateDigest,
    /// One runtime id is provided by two package ids, or one package id
    /// provides two runtime ids.
    #[error("package and runtime identities are not one-to-one")]
    IdentityConflict,
    /// The entry's digest is also on the revoked digest list.
    #[error("entry digest is revoked")]
    RevokedDigest,
}

/// Why a catalog was rejected.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CatalogError {
    /// The input exceeds [`MAX_CATALOG_BYTES`].
    #[error("catalog exceeds the size limit")]
    TooLarge,
    /// The input is not one well-formed JSON value.
    #[error("catalog is not well-formed JSON")]
    Syntax,
    /// An object repeats a key.
    #[error("catalog repeats a JSON object key")]
    DuplicateKey,
    /// A number is negative, fractional or in exponent form.
    #[error("catalog uses a number outside the non-negative integer subset")]
    UnsupportedNumber,
    /// The document does not match the catalog schema (missing, unknown or
    /// mistyped field, or an invalid identifier or digest).
    #[error("catalog does not match the schema")]
    Schema,
    /// The schema version is not supported.
    #[error("unsupported catalog schema version")]
    UnsupportedSchemaVersion,
    /// A bounded collection is too large.
    #[error("catalog exceeds a collection limit: {0}")]
    Limit(CatalogLimit),
    /// A key or signature field is not valid hex of the right length, or the
    /// key is not a usable Ed25519 public key.
    #[error("catalog contains a malformed key or signature")]
    MalformedKeyMaterial,
    /// A key record's id does not match its public key.
    #[error("key record id does not match its public key")]
    KeyIdMismatch,
    /// Two keys share an id.
    #[error("key id is used twice")]
    DuplicateKeyId,
    /// A key record has an empty validity window.
    #[error("key validity window is empty")]
    EmptyKeyWindow,
    /// A key record is endorsed by a key that is neither a root nor an earlier
    /// record.
    #[error("key record endorser is unknown")]
    UnknownEndorser,
    /// A key record's endorsement signature does not verify.
    #[error("key record endorsement does not verify")]
    BadEndorsement,
    /// The catalog carries no signatures.
    #[error("catalog has no signatures")]
    NoSignatures,
    /// Two signatures use the same key id.
    #[error("catalog signatures repeat a key id")]
    DuplicateSignature,
    /// No signature comes from a key in the trust anchor or key chain.
    #[error("catalog is not signed by a known key")]
    UnknownSigner,
    /// The signing key, or a key it chains from, is not yet valid.
    #[error("signing key is not yet valid")]
    SignerNotYetValid,
    /// The signing key, or a key it chains from, has expired.
    #[error("signing key has expired")]
    SignerExpired,
    /// The signing key, or a key it chains from, is revoked.
    #[error("signing key is revoked")]
    SignerRevoked,
    /// A signature does not verify over the canonical catalog bytes.
    #[error("catalog signature does not verify")]
    BadSignature,
    /// The catalog sequence is older than the caller's high-water mark.
    #[error("catalog sequence {sequence} is older than high-water mark {high_water}")]
    StaleSequence {
        /// Sequence of the rejected catalog.
        sequence: u64,
        /// Highest sequence the caller has accepted before.
        high_water: u64,
    },
    /// The catalog's `expires_at` has passed.
    #[error("catalog has expired")]
    Expired,
    /// An entry is invalid; `index` is its position in `entries`.
    #[error("catalog entry {index} is invalid: {reason}")]
    Entry {
        /// Position of the entry.
        index: usize,
        /// Violated rule.
        reason: CatalogEntryRejection,
    },
}

impl From<JsonError> for CatalogError {
    fn from(error: JsonError) -> Self {
        match error {
            JsonError::Syntax => Self::Syntax,
            JsonError::DuplicateKey => Self::DuplicateKey,
            JsonError::UnsupportedNumber => Self::UnsupportedNumber,
        }
    }
}

/// Why a trust anchor could not be built.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum TrustAnchorError {
    /// A root public key is not a usable Ed25519 key.
    #[error("root public key is not a usable Ed25519 key")]
    MalformedKey,
    /// A root validity window is empty.
    #[error("root validity window is empty")]
    EmptyWindow,
    /// The anchor has no roots.
    #[error("trust anchor has no roots")]
    NoRoots,
    /// The anchor lists more than [`MAX_TRUST_ROOTS`] roots or more than
    /// [`MAX_REVOKED_KEYS`] revoked ids.
    #[error("trust anchor is too large")]
    TooLarge,
    /// Two roots share a key.
    #[error("trust anchor repeats a root key")]
    DuplicateRoot,
}

/// One trusted root public key with its validity window.
#[derive(Clone, Debug)]
pub struct RootKey {
    key: VerifyingKey,
    id: KeyId,
    not_before: u64,
    not_after: u64,
}

impl RootKey {
    /// Builds a root from raw public key bytes valid on
    /// `not_before <= now < not_after` (Unix seconds).
    ///
    /// # Errors
    ///
    /// Returns [`TrustAnchorError::MalformedKey`] for an invalid or weak
    /// public key and [`TrustAnchorError::EmptyWindow`] when
    /// `not_after <= not_before`.
    pub fn new(
        public_key: [u8; PUBLIC_KEY_BYTES],
        not_before: u64,
        not_after: u64,
    ) -> Result<Self, TrustAnchorError> {
        let key = VerifyingKey::from_bytes(&public_key)
            .map_err(|_cause| TrustAnchorError::MalformedKey)?;
        if key.is_weak() {
            return Err(TrustAnchorError::MalformedKey);
        }
        if not_after <= not_before {
            return Err(TrustAnchorError::EmptyWindow);
        }
        Ok(Self {
            id: KeyId::derive(&key),
            key,
            not_before,
            not_after,
        })
    }

    /// The root's key id.
    #[must_use]
    pub fn id(&self) -> &KeyId {
        &self.id
    }
}

/// Root keys and revocations the caller trusts independently of any catalog.
///
/// The production anchor is supplied by the release bundle; this crate embeds
/// none.
#[derive(Clone, Debug)]
pub struct TrustAnchor {
    roots: Vec<RootKey>,
    revoked: BTreeSet<KeyId>,
}

impl TrustAnchor {
    /// Builds an anchor from roots and the key ids revoked so far.
    ///
    /// # Errors
    ///
    /// Returns a [`TrustAnchorError`] for no roots, more roots or revoked ids
    /// than the limits allow, or a repeated root.
    pub fn new(roots: Vec<RootKey>, revoked: Vec<KeyId>) -> Result<Self, TrustAnchorError> {
        if roots.is_empty() {
            return Err(TrustAnchorError::NoRoots);
        }
        if roots.len() > MAX_TRUST_ROOTS || revoked.len() > MAX_REVOKED_KEYS {
            return Err(TrustAnchorError::TooLarge);
        }
        let mut ids = BTreeSet::new();
        if !roots.iter().all(|root| ids.insert(root.id.clone())) {
            return Err(TrustAnchorError::DuplicateRoot);
        }
        Ok(Self {
            roots,
            revoked: revoked.into_iter().collect(),
        })
    }
}

/// Canonical message a catalog signature covers: the domain prefix followed by
/// the canonical JSON bytes of the catalog body.
///
/// # Errors
///
/// Returns [`CatalogError::Schema`] if the catalog cannot be serialized.
pub fn catalog_signing_message(catalog: &Catalog) -> Result<Vec<u8>, CatalogError> {
    let value = serde_json::to_value(catalog).map_err(|_cause| CatalogError::Schema)?;
    Ok(domain_message(CATALOG_DOMAIN, &value))
}

/// Canonical message an endorsement covers: the domain prefix followed by the
/// canonical JSON bytes of the record without its `signature`.
#[must_use]
pub fn key_record_signing_message(record: &SignedKeyRecord) -> Vec<u8> {
    let mut body = serde_json::Map::new();
    body.insert("key_id".into(), Value::String(record.key_id.0.clone()));
    body.insert(
        "public_key".into(),
        Value::String(record.public_key.clone()),
    );
    body.insert("not_before".into(), Value::from(record.not_before));
    body.insert("not_after".into(), Value::from(record.not_after));
    body.insert(
        "endorsed_by".into(),
        Value::String(record.endorsed_by.0.clone()),
    );
    domain_message(KEY_RECORD_DOMAIN, &Value::Object(body))
}

fn domain_message(domain: &[u8], value: &Value) -> Vec<u8> {
    let mut message = domain.to_vec();
    message.extend_from_slice(&canonical_bytes(value));
    message
}

/// A key trusted for this verification, with the keys it chains from.
struct TrustedKey {
    key: VerifyingKey,
    not_before: u64,
    not_after: u64,
    /// Ids of the key and every ancestor up to its root, nearest first.
    lineage: Vec<KeyId>,
}

/// Verifies `bytes` as a signed runtime catalog.
///
/// `now` is Unix seconds. `high_water_mark` is the highest catalog sequence
/// the caller has accepted before; a catalog with a lower sequence is
/// rejected as a rollback (an equal sequence is accepted so an installed
/// catalog can be re-verified).
///
/// # Errors
///
/// Returns the first violated rule as a [`CatalogError`].
pub fn verify_catalog(
    bytes: &[u8],
    anchor: &TrustAnchor,
    now: u64,
    high_water_mark: Option<u64>,
) -> Result<VerifiedCatalog, CatalogError> {
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(CatalogError::TooLarge);
    }
    let value = parse_strict(bytes)?;
    let envelope: CatalogEnvelope =
        serde_json::from_value(value.clone()).map_err(|_cause| CatalogError::Schema)?;
    if envelope.schema_version != CATALOG_SCHEMA_VERSION
        || envelope.catalog.schema_version != CATALOG_SCHEMA_VERSION
    {
        return Err(CatalogError::UnsupportedSchemaVersion);
    }
    check_limits(&envelope)?;

    // The signature covers the member exactly as parsed, not a re-serialization
    // of the typed struct.
    let signed_member = value.get("catalog").ok_or(CatalogError::Schema)?;
    let message = domain_message(CATALOG_DOMAIN, signed_member);

    let trusted = build_key_set(&envelope.key_chain, anchor)?;
    let revoked: BTreeSet<&KeyId> = anchor
        .revoked
        .iter()
        .chain(envelope.catalog.revoked_key_ids.iter())
        .collect();
    let signer = verify_signatures(&envelope.signatures, &trusted, &revoked, &message, now)?;

    let catalog = envelope.catalog;
    if let Some(high_water) = high_water_mark {
        if catalog.sequence < high_water {
            return Err(CatalogError::StaleSequence {
                sequence: catalog.sequence,
                high_water,
            });
        }
    }
    if now >= catalog.expires_at {
        return Err(CatalogError::Expired);
    }
    let entries = validate_entries(&catalog)?;
    Ok(VerifiedCatalog {
        sequence: catalog.sequence,
        expires_at: catalog.expires_at,
        signer,
        entries,
        revoked_digests: catalog.revoked_digests.into_iter().collect(),
        revoked_key_ids: catalog.revoked_key_ids.into_iter().collect(),
    })
}

fn check_limits(envelope: &CatalogEnvelope) -> Result<(), CatalogError> {
    let checks = [
        (
            envelope.catalog.entries.len(),
            MAX_CATALOG_ENTRIES,
            CatalogLimit::Entries,
        ),
        (
            envelope.key_chain.len(),
            MAX_KEY_RECORDS,
            CatalogLimit::KeyRecords,
        ),
        (
            envelope.signatures.len(),
            MAX_SIGNATURES,
            CatalogLimit::Signatures,
        ),
        (
            envelope.catalog.revoked_key_ids.len(),
            MAX_REVOKED_KEYS,
            CatalogLimit::RevokedKeys,
        ),
        (
            envelope.catalog.revoked_digests.len(),
            MAX_REVOKED_DIGESTS,
            CatalogLimit::RevokedDigests,
        ),
    ];
    for (len, max, limit) in checks {
        if len > max {
            return Err(CatalogError::Limit(limit));
        }
    }
    Ok(())
}

fn parse_signature(text: &str) -> Result<Signature, CatalogError> {
    decode_hex::<SIGNATURE_BYTES>(text)
        .map(|bytes| Signature::from_bytes(&bytes))
        .ok_or(CatalogError::MalformedKeyMaterial)
}

/// Resolves every root and key record into a trusted key, verifying each
/// endorsement. Records must appear after their endorser.
fn build_key_set(
    chain: &[SignedKeyRecord],
    anchor: &TrustAnchor,
) -> Result<BTreeMap<KeyId, TrustedKey>, CatalogError> {
    let mut keys: BTreeMap<KeyId, TrustedKey> = BTreeMap::new();
    for root in &anchor.roots {
        keys.insert(
            root.id.clone(),
            TrustedKey {
                key: root.key,
                not_before: root.not_before,
                not_after: root.not_after,
                lineage: vec![root.id.clone()],
            },
        );
    }
    for record in chain {
        let public = decode_hex::<PUBLIC_KEY_BYTES>(&record.public_key)
            .ok_or(CatalogError::MalformedKeyMaterial)?;
        let key = VerifyingKey::from_bytes(&public)
            .map_err(|_cause| CatalogError::MalformedKeyMaterial)?;
        if key.is_weak() {
            return Err(CatalogError::MalformedKeyMaterial);
        }
        if KeyId::derive(&key) != record.key_id {
            return Err(CatalogError::KeyIdMismatch);
        }
        if keys.contains_key(&record.key_id) {
            return Err(CatalogError::DuplicateKeyId);
        }
        if record.not_after <= record.not_before {
            return Err(CatalogError::EmptyKeyWindow);
        }
        let endorser = keys
            .get(&record.endorsed_by)
            .ok_or(CatalogError::UnknownEndorser)?;
        let signature = parse_signature(&record.signature)?;
        endorser
            .key
            .verify_strict(&key_record_signing_message(record), &signature)
            .map_err(|_cause| CatalogError::BadEndorsement)?;
        let mut lineage = vec![record.key_id.clone()];
        lineage.extend(endorser.lineage.iter().cloned());
        keys.insert(
            record.key_id.clone(),
            TrustedKey {
                key,
                not_before: record.not_before,
                not_after: record.not_after,
                lineage,
            },
        );
    }
    Ok(keys)
}

/// Returns the id of the first signature that verifies from a key that is
/// valid, unrevoked and unrevoked-in-lineage at `now`; otherwise the first
/// failure.
fn verify_signatures(
    signatures: &[CatalogSignature],
    keys: &BTreeMap<KeyId, TrustedKey>,
    revoked: &BTreeSet<&KeyId>,
    message: &[u8],
    now: u64,
) -> Result<KeyId, CatalogError> {
    if signatures.is_empty() {
        return Err(CatalogError::NoSignatures);
    }
    let mut seen = BTreeSet::new();
    if !signatures.iter().all(|entry| seen.insert(&entry.key_id)) {
        return Err(CatalogError::DuplicateSignature);
    }
    let mut first_failure = None;
    for entry in signatures {
        match verify_one(entry, keys, revoked, message, now) {
            Ok(()) => return Ok(entry.key_id.clone()),
            Err(error) => {
                first_failure.get_or_insert(error);
            }
        }
    }
    Err(first_failure.unwrap_or(CatalogError::UnknownSigner))
}

fn verify_one(
    entry: &CatalogSignature,
    keys: &BTreeMap<KeyId, TrustedKey>,
    revoked: &BTreeSet<&KeyId>,
    message: &[u8],
    now: u64,
) -> Result<(), CatalogError> {
    let trusted = keys.get(&entry.key_id).ok_or(CatalogError::UnknownSigner)?;
    for id in &trusted.lineage {
        if revoked.contains(id) {
            return Err(CatalogError::SignerRevoked);
        }
        let ancestor = keys.get(id).ok_or(CatalogError::UnknownSigner)?;
        if now < ancestor.not_before {
            return Err(CatalogError::SignerNotYetValid);
        }
        if now >= ancestor.not_after {
            return Err(CatalogError::SignerExpired);
        }
    }
    let signature = parse_signature(&entry.signature)?;
    trusted
        .key
        .verify_strict(message, &signature)
        .map_err(|_cause| CatalogError::BadSignature)
}

/// A catalog entry whose fields passed validation.
#[derive(Clone, Debug)]
pub struct VerifiedEntry {
    package_id: PackageId,
    runtime_id: RuntimeId,
    version: PackageVersion,
    digest: PackageDigest,
    platforms: Vec<String>,
    core: VersionReq,
}

impl VerifiedEntry {
    /// Package identity.
    #[must_use]
    pub fn package_id(&self) -> &PackageId {
        &self.package_id
    }

    /// Runtime the package provides.
    #[must_use]
    pub fn runtime_id(&self) -> &RuntimeId {
        &self.runtime_id
    }

    /// Package version.
    #[must_use]
    pub fn version(&self) -> &PackageVersion {
        &self.version
    }

    /// Archive digest the catalog binds to this package.
    #[must_use]
    pub fn digest(&self) -> &PackageDigest {
        &self.digest
    }

    /// Supported platforms.
    #[must_use]
    pub fn platforms(&self) -> &[String] {
        &self.platforms
    }

    /// Whether the package supports `platform`.
    #[must_use]
    pub fn supports_platform(&self, platform: &str) -> bool {
        self.platforms.iter().any(|candidate| candidate == platform)
    }

    /// Whether the package supports core version `core`.
    #[must_use]
    pub fn supports_core(&self, core: &Version) -> bool {
        self.core.matches(core)
    }
}

fn valid_platform(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_PLATFORM_BYTES
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
}

fn validate_entries(catalog: &Catalog) -> Result<Vec<VerifiedEntry>, CatalogError> {
    let revoked: BTreeSet<&PackageDigest> = catalog.revoked_digests.iter().collect();
    let mut versions = BTreeSet::new();
    let mut digests = BTreeSet::new();
    let mut runtime_owner: BTreeMap<&RuntimeId, &PackageId> = BTreeMap::new();
    let mut package_runtime: BTreeMap<&PackageId, &RuntimeId> = BTreeMap::new();
    let mut verified = Vec::with_capacity(catalog.entries.len());
    for (index, entry) in catalog.entries.iter().enumerate() {
        let reject = |reason| CatalogError::Entry { index, reason };
        if entry.runtime_id.as_str() == RuntimeId::SHELL {
            return Err(reject(CatalogEntryRejection::ReservedShell));
        }
        if entry.platforms.is_empty()
            || entry.platforms.len() > MAX_PLATFORMS_PER_ENTRY
            || !entry.platforms.iter().all(|name| valid_platform(name))
            || entry.platforms.iter().collect::<BTreeSet<_>>().len() != entry.platforms.len()
        {
            return Err(reject(CatalogEntryRejection::Platforms));
        }
        let core = parse_core_range(&entry.core)
            .ok_or_else(|| reject(CatalogEntryRejection::CoreRange))?;
        if !versions.insert((&entry.package_id, &entry.version)) {
            return Err(reject(CatalogEntryRejection::DuplicateVersion));
        }
        if !digests.insert(&entry.digest) {
            return Err(reject(CatalogEntryRejection::DuplicateDigest));
        }
        if runtime_owner
            .insert(&entry.runtime_id, &entry.package_id)
            .is_some_and(|owner| owner != &entry.package_id)
            || package_runtime
                .insert(&entry.package_id, &entry.runtime_id)
                .is_some_and(|runtime| runtime != &entry.runtime_id)
        {
            return Err(reject(CatalogEntryRejection::IdentityConflict));
        }
        if revoked.contains(&entry.digest) {
            return Err(reject(CatalogEntryRejection::RevokedDigest));
        }
        verified.push(VerifiedEntry {
            package_id: entry.package_id.clone(),
            runtime_id: entry.runtime_id.clone(),
            version: entry.version.clone(),
            digest: entry.digest.clone(),
            platforms: entry.platforms.clone(),
            core,
        });
    }
    Ok(verified)
}

fn parse_core_range(text: &str) -> Option<VersionReq> {
    if text.is_empty() || text.len() > MAX_CORE_RANGE_BYTES {
        return None;
    }
    let requirement = VersionReq::parse(text).ok()?;
    (!requirement.comparators.is_empty()).then_some(requirement)
}

/// Outcome of asking a [`VerifiedCatalog`] to authorize a package.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Authorization {
    /// The signed catalog binds this exact package id, runtime id and digest.
    Official,
    /// The catalog does not authorize the claim, including any claim on
    /// `shell` and any revoked digest.
    NotAuthorized,
}

/// A catalog whose signature, key chain, freshness and entries were verified.
#[derive(Clone, Debug)]
pub struct VerifiedCatalog {
    sequence: u64,
    expires_at: u64,
    signer: KeyId,
    entries: Vec<VerifiedEntry>,
    revoked_digests: BTreeSet<PackageDigest>,
    revoked_key_ids: BTreeSet<KeyId>,
}

impl VerifiedCatalog {
    /// Publication sequence; the caller persists it as the next high-water
    /// mark.
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Unix second from which the catalog is no longer acceptable.
    #[must_use]
    pub fn expires_at(&self) -> u64 {
        self.expires_at
    }

    /// Id of the key whose signature was accepted.
    #[must_use]
    pub fn signer(&self) -> &KeyId {
        &self.signer
    }

    /// Validated entries in catalog order.
    #[must_use]
    pub fn entries(&self) -> &[VerifiedEntry] {
        &self.entries
    }

    /// Key ids this catalog revokes; the caller adds them to its anchor's
    /// revoked list for later verifications.
    #[must_use]
    pub fn revoked_key_ids(&self) -> &BTreeSet<KeyId> {
        &self.revoked_key_ids
    }

    /// Whether this catalog revokes `digest`.
    #[must_use]
    pub fn is_digest_revoked(&self, digest: &PackageDigest) -> bool {
        self.revoked_digests.contains(digest)
    }

    /// Decides whether the catalog authorizes `package_id` to provide
    /// `runtime_id` with the archive `digest`.
    ///
    /// Only an entry that matches all three yields [`Authorization::Official`],
    /// which is also the only way a package gets `codex`, `claude` or `hermes`.
    #[must_use]
    pub fn authorize(
        &self,
        package_id: &PackageId,
        runtime_id: &RuntimeId,
        digest: &PackageDigest,
    ) -> Authorization {
        if runtime_id.as_str() == RuntimeId::SHELL || self.is_digest_revoked(digest) {
            return Authorization::NotAuthorized;
        }
        let matched = self.entries.iter().any(|entry| {
            &entry.digest == digest
                && &entry.package_id == package_id
                && &entry.runtime_id == runtime_id
        });
        if matched {
            Authorization::Official
        } else {
            Authorization::NotAuthorized
        }
    }
}

/// Why a local trust decision refused a package.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum LocalTrustError {
    /// The archive digest differs from the owner-supplied digest.
    #[error("archive digest does not match the owner-supplied digest")]
    DigestMismatch,
    /// The runtime id is `shell` or an official alias; only the signed catalog
    /// can authorize those.
    #[error("runtime id is reserved for the official catalog")]
    ReservedRuntime,
}

/// A local trust grant that is not catalog authorization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalAuthorization {
    /// The owner trusted exactly this archive digest on this host.
    Local,
}

/// Trust the owner grants a third-party archive without a catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalTrust {
    /// Trust exactly the archive with this digest.
    ExplicitDigest(PackageDigest),
}

impl LocalTrust {
    /// Authorizes an archive that claims `runtime_id` and hashes to `digest`.
    ///
    /// The result is [`LocalAuthorization::Local`], a type that cannot be
    /// confused with [`Authorization::Official`]; reserved runtime ids are
    /// refused outright.
    ///
    /// # Errors
    ///
    /// Returns [`LocalTrustError::ReservedRuntime`] for `shell` and the
    /// official aliases, and [`LocalTrustError::DigestMismatch`] when `digest`
    /// differs from the owner-supplied one.
    pub fn authorize(
        &self,
        runtime_id: &RuntimeId,
        digest: &PackageDigest,
    ) -> Result<LocalAuthorization, LocalTrustError> {
        let Self::ExplicitDigest(trusted) = self;
        let reserved = runtime_id.as_str() == RuntimeId::SHELL
            || OFFICIAL_RUNTIMES.contains(&runtime_id.as_str());
        if reserved {
            return Err(LocalTrustError::ReservedRuntime);
        }
        if trusted != digest {
            return Err(LocalTrustError::DigestMismatch);
        }
        Ok(LocalAuthorization::Local)
    }
}
