//! Signed plugin index: format, trust model and verification.
//!
//! The plugin index authorizes plugin packages by archive digest. It is a
//! separate document family from the runtime catalog: its own file name, its
//! own signature domains and its own verifier, so a catalog signature never
//! verifies as an index signature and the reverse.
//!
//! Trust is exactly two levels deep. The caller names one `endorser` root.
//! That root endorses index key records directly (scope `official`) and signs
//! the revocation list embedded in every index document. An index signature
//! counts only when it comes from such a record; a root key never signs an
//! index. [`verify_index`] never consults the catalog key chain.
//!
//! The verifier is pure and clock-free: `now` and the persisted marks
//! ([`IndexVerifierState`]) are explicit inputs. Document validity
//! ([`verify_index`]) is separate from per-entry install authorization
//! ([`VerifiedIndex::authorize`]), so an entry this core cannot use never
//! invalidates the index for its other entries.
//!
//! The format is described in `docs/knowledge/concepts/runtime-catalog.md`.

// Rust guideline compliant 2026-06-26

use std::collections::{BTreeMap, BTreeSet};

use ed25519_dalek::VerifyingKey;
use protocol::{PackageDigest, PackageId, PackageVersion, RuntimeId};
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::canonical_json::{parse_strict, JsonError};
use crate::catalog::{
    decode_hex, domain_message, parse_core_range, valid_platform, CatalogSignature, KeyId, RootKey,
    MAX_CORE_RANGE_BYTES, MAX_PLATFORMS_PER_ENTRY, MAX_REVOKED_KEYS, PUBLIC_KEY_BYTES,
    SIGNATURE_BYTES,
};

/// Schema version 1: every entry is a runtime package.
pub const INDEX_SCHEMA_VERSION: u64 = 1;

/// Schema version 2: entries carry a `kind`, `runtime` or `context`.
///
/// A client that reads only schema 1 refuses a schema 2 document as a whole,
/// so schema 2 is published under its own file name beside the schema 1 file.
pub const INDEX_SCHEMA_VERSION_2: u64 = 2;

/// Every schema version this build reads.
pub const SUPPORTED_INDEX_SCHEMA_VERSIONS: [u64; 2] =
    [INDEX_SCHEMA_VERSION, INDEX_SCHEMA_VERSION_2];

/// Published file name of the schema 1 document, which lists runtime packages
/// only; each schema publishes its own file name.
pub const OFFICIAL_INDEX_FILE_NAME: &str = "official-index-v1.json";

/// Published file name of the schema 2 document.
pub const OFFICIAL_INDEX_FILE_NAME_V2: &str = "official-index-v2.json";

/// Largest accepted index document: 1 MiB, checked before parsing.
pub const MAX_INDEX_BYTES: usize = 1024 * 1024;

/// Largest accepted number of index entries: 256.
pub const MAX_INDEX_ENTRIES: usize = 256;

/// Largest accepted number of index key records: 4.
///
/// One key signs at a time; a rotation overlap adds one more.
pub const MAX_INDEX_KEY_RECORDS: usize = 4;

/// Largest accepted number of index signatures: 4, one per key record.
pub const MAX_INDEX_SIGNATURES: usize = 4;

/// Largest accepted entry URL: 512 bytes.
pub const MAX_ENTRY_URL_BYTES: usize = 512;

/// Longest endorsement window of an index key: 366 days.
pub const MAX_INDEX_KEY_WINDOW_SECONDS: u64 = 366 * 24 * 60 * 60;

/// How far an index sequence (unix seconds of signing) may lead the host
/// clock: 1 day.
///
/// Bounds how far a stolen index key can raise a host's high-water mark, so
/// the next legitimate document, which revokes that key, still gets through.
pub const MAX_INDEX_SEQUENCE_SKEW_SECONDS: u64 = 24 * 60 * 60;

/// Longest `expires_at - sequence` of an index document: 14 days.
pub const MAX_INDEX_LIFETIME_SECONDS: u64 = 14 * 24 * 60 * 60;

/// An entry's package id is exactly this prefix plus its runtime id.
///
/// Every release package id (`pohunek.runtime.codex`, `.claude`, `.hermes`,
/// `.pi`) has this shape, so the binding keeps an entry from claiming one.
pub const OFFICIAL_RUNTIME_PACKAGE_PREFIX: &str = "pohunek.runtime.";

/// A context package id is exactly this prefix plus a non-empty tail.
///
/// The prefix belongs to `context` entries; a `runtime` entry never carries it.
pub const OFFICIAL_CONTEXT_PACKAGE_PREFIX: &str = "pohunek.context.";

/// The only scope an index key record and an index document carry.
pub const OFFICIAL_SCOPE: &str = "official";

/// Namespace every authorized package id lives in.
pub const OFFICIAL_NAMESPACE: &str = "pohunek.";

/// Runtime ids an index entry never authorizes: the shell, the reserved
/// aliases, and every runtime the release catalog serves.
pub const INDEX_REFUSED_RUNTIME_IDS: [&str; 5] = ["shell", "codex", "claude", "hermes", "pi"];

/// Domain separator of the schema 1 index body signature.
const INDEX_DOMAIN: &[u8] = b"pohunek-plugin-index-v1\n";

/// Domain separator of the schema 2 index body signature, so a signature of one
/// schema never verifies a body of the other.
const INDEX_DOMAIN_V2: &[u8] = b"pohunek-plugin-index-v2\n";

/// The signature domain of a body of schema `version`.
fn index_domain(version: u64) -> &'static [u8] {
    if version == INDEX_SCHEMA_VERSION_2 {
        INDEX_DOMAIN_V2
    } else {
        INDEX_DOMAIN
    }
}

/// Domain separator of an index key record endorsement.
const INDEX_KEY_RECORD_DOMAIN: &[u8] = b"pohunek-plugin-index-key-record-v1\n";

/// Domain separator of the embedded revocation list signature; it names its
/// document family so no other revocation document can share it.
const REVOCATION_DOMAIN: &[u8] = b"pohunek-plugin-index-revocations-v1\n";

/// What an index entry authorizes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexEntryKind {
    /// A runtime package; the entry names the runtime it provides.
    Runtime,
    /// A context package: files without a runtime or commands.
    Context,
}

/// One plugin package version the index authorizes.
///
/// A schema 1 entry has no `kind` and a `runtime_id`; a schema 2 entry has a
/// `kind`, and a `runtime_id` exactly when the kind is `runtime`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IndexEntry {
    /// Package identity.
    pub package_id: PackageId,
    /// What the entry authorizes; absent in schema 1, where it is `runtime`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<IndexEntryKind>,
    /// Runtime the package provides; absent for a `context` entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_id: Option<RuntimeId>,
    /// Package version.
    pub version: PackageVersion,
    /// Digest of the package archive bytes.
    pub digest: PackageDigest,
    /// Where the archive is downloaded from.
    pub url: String,
    /// Platforms the package supports, as lowercase `[a-z0-9._-]` names.
    pub platforms: Vec<String>,
    /// Semver range of core versions the package supports. It is parsed at
    /// install time, not when the index is verified.
    pub core: String,
}

impl IndexEntry {
    /// What the entry authorizes: its `kind`, `runtime` when it has none.
    #[must_use]
    pub fn kind(&self) -> IndexEntryKind {
        self.kind.unwrap_or(IndexEntryKind::Runtime)
    }

    /// Whether the entry lists `platform` and its core range parses and
    /// matches `core`. The same two checks [`VerifiedIndex::authorize`] ends
    /// with, so a client that selects by this predicate never picks an entry
    /// the verifier would refuse for those reasons.
    #[must_use]
    pub fn supports(&self, core: &Version, platform: &str) -> bool {
        parse_core_range(&self.core).is_some_and(|range| range.matches(core))
            && self.platforms.iter().any(|listed| listed == platform)
    }
}

/// The signed index body.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IndexBody {
    /// A version in [`SUPPORTED_INDEX_SCHEMA_VERSIONS`], equal to the
    /// envelope's.
    pub schema_version: u64,
    /// Must equal [`OFFICIAL_SCOPE`].
    pub scope: String,
    /// Unix seconds of signing; the anti-rollback counter.
    pub sequence: u64,
    /// Unix seconds from which the document must no longer be accepted.
    pub expires_at: u64,
    /// Authorized package versions.
    pub entries: Vec<IndexEntry>,
}

/// An index signing key introduced by the endorser root.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IndexKeyRecord {
    /// Id derived from `public_key`.
    pub key_id: KeyId,
    /// 64 lowercase hex characters of the raw Ed25519 public key.
    pub public_key: String,
    /// Must equal [`OFFICIAL_SCOPE`].
    pub scope: String,
    /// First Unix second the key may sign.
    pub not_before: u64,
    /// Unix second from which the key may no longer sign.
    pub not_after: u64,
    /// Id of the endorser root.
    pub endorsed_by: KeyId,
    /// 128 lowercase hex characters: the endorser's signature over
    /// [`index_key_record_signing_message`] of this record.
    pub signature: String,
}

/// The revocation list the endorser root signs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RevocationBody {
    /// A version in [`SUPPORTED_INDEX_SCHEMA_VERSIONS`]. The list's format is
    /// the same in every schema, so a document embeds the list as signed,
    /// whatever its own schema.
    pub schema_version: u64,
    /// Publication counter of the revocation list.
    pub sequence: u64,
    /// Keys that must not sign or endorse, in addition to the caller's.
    pub revoked_key_ids: Vec<KeyId>,
}

/// A revocation list with the endorser's signature.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignedRevocations {
    /// The signed body.
    pub revocations: RevocationBody,
    /// The endorser's signature over [`revocation_signing_message`].
    pub signature: CatalogSignature,
}

/// The complete `official-index-v1.json` document.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IndexEnvelope {
    /// A version in [`SUPPORTED_INDEX_SCHEMA_VERSIONS`].
    pub schema_version: u64,
    /// The signed body.
    pub index: IndexBody,
    /// Index key records, each endorsed by the endorser root.
    pub key_records: Vec<IndexKeyRecord>,
    /// Detached signatures over the index body, each from a key record.
    pub signatures: Vec<CatalogSignature>,
    /// The embedded, root-signed revocation list.
    pub revocations: SignedRevocations,
}

/// What the caller has persisted from earlier verifications.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IndexVerifierState {
    /// Highest index sequence accepted before.
    pub index_high_water: Option<u64>,
    /// Highest revocation list sequence accepted before.
    pub revocation_high_water: Option<u64>,
    /// Key ids revoked by earlier documents.
    pub revoked_key_ids: BTreeSet<KeyId>,
}

/// Which bounded collection exceeded its limit.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum IndexLimit {
    /// Too many index entries.
    #[error("entries")]
    Entries,
    /// Too many index key records.
    #[error("key records")]
    KeyRecords,
    /// Too many signatures.
    #[error("signatures")]
    Signatures,
    /// Too many revoked key ids.
    #[error("revoked keys")]
    RevokedKeys,
}

/// Why one index entry is structurally invalid.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum IndexEntryRejection {
    /// The platform list is empty, too long, has a bad name or repeats a name.
    #[error("platform list is invalid")]
    Platforms,
    /// The URL is empty, too long, or not printable ASCII without whitespace.
    #[error("url is invalid")]
    Url,
    /// The core range string is empty or too long.
    #[error("core version range is empty or too long")]
    Core,
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
    /// The `kind` and the `runtime_id` of the entry do not fit its schema
    /// version: schema 1 has no `kind`, schema 2 has one, and only a `runtime`
    /// entry has a `runtime_id`.
    #[error("entry kind and runtime id do not fit the schema")]
    Kind,
    /// The package id carries the prefix of the other kind, or a `context`
    /// entry's id is not [`OFFICIAL_CONTEXT_PACKAGE_PREFIX`] plus a tail.
    #[error("package id does not fit the entry kind")]
    PackageIdKind,
}

/// Why an index document was rejected. Carries no document content.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum IndexError {
    /// The document is larger than [`MAX_INDEX_BYTES`].
    #[error("plugin index is too large")]
    TooLarge,
    /// The bytes are not a single well-formed JSON value.
    #[error("plugin index is not valid JSON")]
    Syntax,
    /// An object has the same key twice.
    #[error("plugin index repeats an object key")]
    DuplicateKey,
    /// A number is negative, fractional or in exponent form.
    #[error("plugin index uses a number outside the non-negative integer subset")]
    UnsupportedNumber,
    /// The document does not match the index schema.
    #[error("plugin index does not match the schema")]
    Schema,
    /// A schema version is not supported.
    #[error("unsupported plugin index schema version")]
    UnsupportedSchemaVersion,
    /// A bounded collection is too large.
    #[error("plugin index exceeds a collection limit: {0}")]
    Limit(IndexLimit),
    /// The endorser root is not yet valid.
    #[error("index endorser key is not yet valid")]
    EndorserNotYetValid,
    /// The endorser root has expired.
    #[error("index endorser key has expired")]
    EndorserExpired,
    /// The revocation list is not signed by the endorser or does not verify.
    #[error("revocation list signature does not verify")]
    BadRevocation,
    /// The revocation list is older than the caller's high-water mark.
    #[error("revocation sequence {sequence} is older than high-water mark {high_water}")]
    StaleRevocation {
        /// Sequence of the rejected revocation list.
        sequence: u64,
        /// Highest revocation sequence the caller has accepted before.
        high_water: u64,
    },
    /// The endorser or the signing key is revoked.
    #[error("key is revoked")]
    SignerRevoked,
    /// A key or signature field is not valid hex of the right length, or the
    /// key is not a usable Ed25519 public key.
    #[error("plugin index contains a malformed key or signature")]
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
    /// A key record's validity window is longer than
    /// [`MAX_INDEX_KEY_WINDOW_SECONDS`].
    #[error("key validity window is too long")]
    KeyWindowTooLong,
    /// A key record or the index body has a scope other than `official`.
    #[error("scope is not official")]
    ScopeMismatch,
    /// A key record is endorsed by a key other than the endorser root.
    #[error("key record endorser is not the designated root")]
    UnknownEndorser,
    /// A key record's endorsement signature does not verify.
    #[error("key record endorsement does not verify")]
    BadEndorsement,
    /// The document carries no signatures.
    #[error("plugin index has no signatures")]
    NoSignatures,
    /// Two signatures use the same key id.
    #[error("plugin index signatures repeat a key id")]
    DuplicateSignature,
    /// No signature comes from an index key record.
    #[error("plugin index is not signed by an index key")]
    UnknownSigner,
    /// The signing key is not yet valid.
    #[error("signing key is not yet valid")]
    SignerNotYetValid,
    /// The signing key has expired.
    #[error("signing key has expired")]
    SignerExpired,
    /// A signature does not verify over the index body.
    #[error("plugin index signature does not verify")]
    BadSignature,
    /// The index sequence is older than the caller's high-water mark.
    #[error("index sequence {sequence} is older than high-water mark {high_water}")]
    StaleSequence {
        /// Sequence of the rejected index.
        sequence: u64,
        /// Highest index sequence the caller has accepted before.
        high_water: u64,
    },
    /// The index sequence leads the host clock by more than
    /// [`MAX_INDEX_SEQUENCE_SKEW_SECONDS`].
    #[error("index sequence is too far in the future")]
    SequenceInFuture,
    /// `expires_at` is not after the sequence, or is more than
    /// [`MAX_INDEX_LIFETIME_SECONDS`] after it.
    #[error("index lifetime is empty or too long")]
    LifetimeTooLong,
    /// The index's `expires_at` has passed.
    #[error("plugin index has expired")]
    Expired,
    /// An entry is structurally invalid; `index` is its position.
    #[error("index entry {index} is invalid: {reason}")]
    Entry {
        /// Position of the entry.
        index: usize,
        /// Violated rule.
        reason: IndexEntryRejection,
    },
}

impl From<JsonError> for IndexError {
    fn from(error: JsonError) -> Self {
        match error {
            JsonError::Syntax => Self::Syntax,
            JsonError::DuplicateKey => Self::DuplicateKey,
            JsonError::UnsupportedNumber => Self::UnsupportedNumber,
        }
    }
}

/// Why an install-time check refused one index entry.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum IndexEntryRefusal {
    /// No entry has this archive digest.
    #[error("archive digest is not listed in the index")]
    NotListed,
    /// The package id, runtime id or version differs from the entry's.
    #[error("package identity differs from the index entry")]
    IdentityMismatch,
    /// The package id is outside the `pohunek.` namespace.
    #[error("package id is outside the official namespace")]
    Namespace,
    /// The runtime id is one an index never authorizes.
    #[error("runtime id is refused for index packages")]
    RuntimeRefused,
    /// The package id is not `pohunek.runtime.` plus the runtime id, or a
    /// context package id is not `pohunek.context.` plus a tail.
    #[error("package id does not match its runtime id")]
    PackageIdBinding,
    /// The entry that lists the digest has the other kind: a runtime entry
    /// never authorizes a context package and the reverse.
    #[error("index entry is of another kind")]
    KindMismatch,
    /// The core range does not parse or constrains nothing.
    #[error("core version range is invalid")]
    CoreRange,
    /// The core range excludes this core version.
    #[error("core version is outside the entry's range")]
    CoreExcluded,
    /// The entry does not list this platform.
    #[error("platform is not listed for the entry")]
    Platform,
}

/// Canonical message an index signature covers: the domain prefix followed by
/// the canonical JSON bytes of the index body.
///
/// # Errors
///
/// Returns [`IndexError::Schema`] if the body cannot be serialized.
pub fn index_signing_message(body: &IndexBody) -> Result<Vec<u8>, IndexError> {
    let value = serde_json::to_value(body).map_err(|_cause| IndexError::Schema)?;
    Ok(domain_message(index_domain(body.schema_version), &value))
}

/// Canonical message an endorsement covers: the domain prefix followed by the
/// canonical JSON bytes of the record without its `signature`.
#[must_use]
pub fn index_key_record_signing_message(record: &IndexKeyRecord) -> Vec<u8> {
    let mut body = serde_json::Map::new();
    body.insert(
        "key_id".into(),
        Value::String(record.key_id.as_str().into()),
    );
    body.insert(
        "public_key".into(),
        Value::String(record.public_key.clone()),
    );
    body.insert("scope".into(), Value::String(record.scope.clone()));
    body.insert("not_before".into(), Value::from(record.not_before));
    body.insert("not_after".into(), Value::from(record.not_after));
    body.insert(
        "endorsed_by".into(),
        Value::String(record.endorsed_by.as_str().into()),
    );
    domain_message(INDEX_KEY_RECORD_DOMAIN, &Value::Object(body))
}

/// Canonical message a revocation list signature covers: the domain prefix
/// followed by the canonical JSON bytes of the revocation body.
///
/// # Errors
///
/// Returns [`IndexError::Schema`] if the body cannot be serialized.
pub fn revocation_signing_message(body: &RevocationBody) -> Result<Vec<u8>, IndexError> {
    let value = serde_json::to_value(body).map_err(|_cause| IndexError::Schema)?;
    Ok(domain_message(REVOCATION_DOMAIN, &value))
}

/// A parsed envelope with the members its signatures cover, as parsed.
struct ParsedIndex {
    envelope: IndexEnvelope,
    index_message: Vec<u8>,
    revocation_message: Vec<u8>,
}

/// Size, strict JSON, schema version, typed decode and limits: the checks that
/// need no key.
fn parse_document(bytes: &[u8]) -> Result<ParsedIndex, IndexError> {
    if bytes.len() > MAX_INDEX_BYTES {
        return Err(IndexError::TooLarge);
    }
    let value = parse_strict(bytes)?;
    // A document of another schema is refused as such before its shape is
    // judged against this schema.
    let declared_other = [
        value.get("schema_version"),
        value.pointer("/index/schema_version"),
        value.pointer("/revocations/revocations/schema_version"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_u64)
    .any(|version| !SUPPORTED_INDEX_SCHEMA_VERSIONS.contains(&version));
    if declared_other {
        return Err(IndexError::UnsupportedSchemaVersion);
    }
    let envelope: IndexEnvelope =
        serde_json::from_value(value.clone()).map_err(|_cause| IndexError::Schema)?;
    let version = envelope.schema_version;
    if !SUPPORTED_INDEX_SCHEMA_VERSIONS.contains(&version)
        || envelope.index.schema_version != version
        || !SUPPORTED_INDEX_SCHEMA_VERSIONS
            .contains(&envelope.revocations.revocations.schema_version)
    {
        return Err(IndexError::UnsupportedSchemaVersion);
    }
    check_limits(&envelope)?;

    // Signatures cover the members exactly as parsed, not a re-serialization
    // of the typed structs.
    let index_member = value.get("index").ok_or(IndexError::Schema)?;
    let revocation_member = value
        .pointer("/revocations/revocations")
        .ok_or(IndexError::Schema)?;
    Ok(ParsedIndex {
        index_message: domain_message(index_domain(version), index_member),
        revocation_message: domain_message(REVOCATION_DOMAIN, revocation_member),
        envelope,
    })
}

fn check_limits(envelope: &IndexEnvelope) -> Result<(), IndexError> {
    let checks = [
        (
            envelope.index.entries.len(),
            MAX_INDEX_ENTRIES,
            IndexLimit::Entries,
        ),
        (
            envelope.key_records.len(),
            MAX_INDEX_KEY_RECORDS,
            IndexLimit::KeyRecords,
        ),
        (
            envelope.signatures.len(),
            MAX_INDEX_SIGNATURES,
            IndexLimit::Signatures,
        ),
        (
            envelope.revocations.revocations.revoked_key_ids.len(),
            MAX_REVOKED_KEYS,
            IndexLimit::RevokedKeys,
        ),
    ];
    for (len, max, limit) in checks {
        if len > max {
            return Err(IndexError::Limit(limit));
        }
    }
    Ok(())
}

/// Parses an index document without checking any signature or freshness.
///
/// Runs the size, strict JSON, schema version, typed decode and limit checks
/// of [`verify_index`] and nothing else. The result is untrusted input meant
/// for messaging only; the verifier is the trust boundary.
///
/// # Errors
///
/// Returns the first violated rule as an [`IndexError`].
pub fn parse_index_envelope(bytes: &[u8]) -> Result<IndexEnvelope, IndexError> {
    parse_document(bytes).map(|parsed| parsed.envelope)
}

/// An index key record whose endorsement verified.
struct TrustedIndexKey {
    key: VerifyingKey,
    not_before: u64,
    not_after: u64,
}

/// Verifies `bytes` as a signed plugin index.
///
/// `endorser` is the one root allowed to endorse index keys and sign the
/// revocation list. `base_revoked` holds key ids revoked outside the index
/// (the anchor's list and the catalog state); `state` holds the persisted
/// marks and the key ids earlier indexes revoked. `now` is Unix seconds. An
/// equal sequence is accepted so an installed index can be re-verified.
///
/// # Errors
///
/// Returns the first violated rule as an [`IndexError`].
pub fn verify_index(
    bytes: &[u8],
    endorser: &RootKey,
    base_revoked: &BTreeSet<KeyId>,
    state: &IndexVerifierState,
    now: u64,
) -> Result<VerifiedIndex, IndexError> {
    let ParsedIndex {
        envelope,
        index_message,
        revocation_message,
    } = parse_document(bytes)?;

    if now < endorser.not_before() {
        return Err(IndexError::EndorserNotYetValid);
    }
    if now >= endorser.not_after() {
        return Err(IndexError::EndorserExpired);
    }

    let revocations = &envelope.revocations;
    verify_revocations(revocations, endorser, &revocation_message)?;
    let revocation_sequence = revocations.revocations.sequence;
    if let Some(high_water) = state.revocation_high_water {
        if revocation_sequence < high_water {
            return Err(IndexError::StaleRevocation {
                sequence: revocation_sequence,
                high_water,
            });
        }
    }

    let revoked: BTreeSet<&KeyId> = base_revoked
        .iter()
        .chain(state.revoked_key_ids.iter())
        .chain(revocations.revocations.revoked_key_ids.iter())
        .collect();
    if revoked.contains(endorser.id()) {
        return Err(IndexError::SignerRevoked);
    }

    let trusted = build_key_set(&envelope.key_records, endorser)?;
    let signer = verify_signatures(
        &envelope.signatures,
        &trusted,
        &revoked,
        &index_message,
        now,
    )?;

    let body = &envelope.index;
    check_body(body, state, now)?;
    check_entries(body.schema_version, &body.entries)?;

    let revoked_key_ids = envelope
        .revocations
        .revocations
        .revoked_key_ids
        .iter()
        .cloned()
        .collect();
    Ok(VerifiedIndex {
        sequence: body.sequence,
        expires_at: body.expires_at,
        revocation_sequence,
        signer,
        revoked_key_ids,
        entries: envelope.index.entries,
    })
}

fn verify_revocations(
    revocations: &SignedRevocations,
    endorser: &RootKey,
    message: &[u8],
) -> Result<(), IndexError> {
    if &revocations.signature.key_id != endorser.id() {
        return Err(IndexError::BadRevocation);
    }
    let signature = decode_hex::<SIGNATURE_BYTES>(&revocations.signature.signature)
        .map(|raw| ed25519_dalek::Signature::from_bytes(&raw))
        .ok_or(IndexError::BadRevocation)?;
    endorser
        .verifying_key()
        .verify_strict(message, &signature)
        .map_err(|_cause| IndexError::BadRevocation)
}

/// Resolves every key record into a trusted key, verifying each endorsement
/// against the endorser root alone.
fn build_key_set(
    records: &[IndexKeyRecord],
    endorser: &RootKey,
) -> Result<BTreeMap<KeyId, TrustedIndexKey>, IndexError> {
    let mut keys: BTreeMap<KeyId, TrustedIndexKey> = BTreeMap::new();
    for record in records {
        let key = check_key_record(record, endorser, keys.contains_key(&record.key_id))?;
        keys.insert(
            record.key_id.clone(),
            TrustedIndexKey {
                key,
                not_before: record.not_before,
                not_after: record.not_after,
            },
        );
    }
    Ok(keys)
}

/// The rules one key record must satisfy under `endorser`, in verification
/// order. `duplicate` says an earlier record of the same document already has
/// this key id.
fn check_key_record(
    record: &IndexKeyRecord,
    endorser: &RootKey,
    duplicate: bool,
) -> Result<VerifyingKey, IndexError> {
    let public = decode_hex::<PUBLIC_KEY_BYTES>(&record.public_key)
        .ok_or(IndexError::MalformedKeyMaterial)?;
    let key =
        VerifyingKey::from_bytes(&public).map_err(|_cause| IndexError::MalformedKeyMaterial)?;
    if key.is_weak() {
        return Err(IndexError::MalformedKeyMaterial);
    }
    if KeyId::derive(&key) != record.key_id {
        return Err(IndexError::KeyIdMismatch);
    }
    if &record.key_id == endorser.id() || duplicate {
        return Err(IndexError::DuplicateKeyId);
    }
    if record.not_after <= record.not_before {
        return Err(IndexError::EmptyKeyWindow);
    }
    if record.not_after - record.not_before > MAX_INDEX_KEY_WINDOW_SECONDS {
        return Err(IndexError::KeyWindowTooLong);
    }
    if record.scope != OFFICIAL_SCOPE {
        return Err(IndexError::ScopeMismatch);
    }
    if &record.endorsed_by != endorser.id() {
        return Err(IndexError::UnknownEndorser);
    }
    let signature = decode_hex::<SIGNATURE_BYTES>(&record.signature)
        .map(|raw| ed25519_dalek::Signature::from_bytes(&raw))
        .ok_or(IndexError::MalformedKeyMaterial)?;
    endorser
        .verifying_key()
        .verify_strict(&index_key_record_signing_message(record), &signature)
        .map_err(|_cause| IndexError::BadEndorsement)?;
    Ok(key)
}

/// Parses `bytes` as one index key record and verifies it under `endorser`
/// with the rules [`verify_index`] applies to each record of a document.
///
/// The endorser's own validity window and the revocation lists are not
/// consulted: this checks the record alone.
///
/// # Errors
///
/// Returns the first violated rule as an [`IndexError`].
pub fn verify_key_record_document(
    bytes: &[u8],
    endorser: &RootKey,
) -> Result<IndexKeyRecord, IndexError> {
    if bytes.len() > MAX_INDEX_BYTES {
        return Err(IndexError::TooLarge);
    }
    let value = parse_strict(bytes)?;
    let record: IndexKeyRecord =
        serde_json::from_value(value).map_err(|_cause| IndexError::Schema)?;
    check_key_record(&record, endorser, false)?;
    Ok(record)
}

/// Parses `bytes` as one signed revocation list and verifies it under
/// `endorser` with the rules [`verify_index`] applies to the embedded list.
///
/// The signature covers the `revocations` member as parsed. The list's
/// sequence is returned for the caller to compare; no persisted mark is
/// consulted.
///
/// # Errors
///
/// Returns the first violated rule as an [`IndexError`].
pub fn verify_revocation_document(
    bytes: &[u8],
    endorser: &RootKey,
) -> Result<SignedRevocations, IndexError> {
    if bytes.len() > MAX_INDEX_BYTES {
        return Err(IndexError::TooLarge);
    }
    let value = parse_strict(bytes)?;
    if value
        .pointer("/revocations/schema_version")
        .and_then(Value::as_u64)
        .is_some_and(|version| !SUPPORTED_INDEX_SCHEMA_VERSIONS.contains(&version))
    {
        return Err(IndexError::UnsupportedSchemaVersion);
    }
    let document: SignedRevocations =
        serde_json::from_value(value.clone()).map_err(|_cause| IndexError::Schema)?;
    if !SUPPORTED_INDEX_SCHEMA_VERSIONS.contains(&document.revocations.schema_version) {
        return Err(IndexError::UnsupportedSchemaVersion);
    }
    if document.revocations.revoked_key_ids.len() > MAX_REVOKED_KEYS {
        return Err(IndexError::Limit(IndexLimit::RevokedKeys));
    }
    let member = value.get("revocations").ok_or(IndexError::Schema)?;
    verify_revocations(
        &document,
        endorser,
        &domain_message(REVOCATION_DOMAIN, member),
    )?;
    Ok(document)
}

/// The message an index signature covers for the document `bytes`: the domain
/// prefix plus the canonical bytes of the `index` member as parsed.
///
/// Runs the key-free checks of [`verify_index`] (size, strict JSON, schema
/// version, typed decode, limits) and nothing else, so a signer outside the
/// workspace signs exactly what the verifier will later check.
///
/// # Errors
///
/// Returns the first violated rule as an [`IndexError`].
pub fn index_document_signing_message(bytes: &[u8]) -> Result<Vec<u8>, IndexError> {
    parse_document(bytes).map(|parsed| parsed.index_message)
}

/// Checks `body` as [`verify_index`] does after the signatures: scope,
/// freshness against `now` (sequence at most a day ahead, lifetime within
/// bounds, not expired) and entry structure. No persisted mark is consulted.
///
/// # Errors
///
/// Returns the first violated rule as an [`IndexError`].
pub fn check_index_body(body: &IndexBody, now: u64) -> Result<(), IndexError> {
    check_body(body, &IndexVerifierState::default(), now)?;
    check_entries(body.schema_version, &body.entries)
}

/// Returns the id of the first signature that verifies from a valid,
/// unrevoked key record; otherwise the first failure. A key id that names no
/// record, a root included, is an unknown signer.
fn verify_signatures(
    signatures: &[CatalogSignature],
    keys: &BTreeMap<KeyId, TrustedIndexKey>,
    revoked: &BTreeSet<&KeyId>,
    message: &[u8],
    now: u64,
) -> Result<KeyId, IndexError> {
    if signatures.is_empty() {
        return Err(IndexError::NoSignatures);
    }
    let mut seen = BTreeSet::new();
    if !signatures.iter().all(|entry| seen.insert(&entry.key_id)) {
        return Err(IndexError::DuplicateSignature);
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
    Err(first_failure.unwrap_or(IndexError::UnknownSigner))
}

fn verify_one(
    entry: &CatalogSignature,
    keys: &BTreeMap<KeyId, TrustedIndexKey>,
    revoked: &BTreeSet<&KeyId>,
    message: &[u8],
    now: u64,
) -> Result<(), IndexError> {
    let trusted = keys.get(&entry.key_id).ok_or(IndexError::UnknownSigner)?;
    if revoked.contains(&entry.key_id) {
        return Err(IndexError::SignerRevoked);
    }
    if now < trusted.not_before {
        return Err(IndexError::SignerNotYetValid);
    }
    if now >= trusted.not_after {
        return Err(IndexError::SignerExpired);
    }
    let signature = decode_hex::<SIGNATURE_BYTES>(&entry.signature)
        .map(|raw| ed25519_dalek::Signature::from_bytes(&raw))
        .ok_or(IndexError::MalformedKeyMaterial)?;
    trusted
        .key
        .verify_strict(message, &signature)
        .map_err(|_cause| IndexError::BadSignature)
}

fn check_body(body: &IndexBody, state: &IndexVerifierState, now: u64) -> Result<(), IndexError> {
    if body.scope != OFFICIAL_SCOPE {
        return Err(IndexError::ScopeMismatch);
    }
    if let Some(high_water) = state.index_high_water {
        if body.sequence < high_water {
            return Err(IndexError::StaleSequence {
                sequence: body.sequence,
                high_water,
            });
        }
    }
    if body.sequence > now.saturating_add(MAX_INDEX_SEQUENCE_SKEW_SECONDS) {
        return Err(IndexError::SequenceInFuture);
    }
    if body.expires_at <= body.sequence
        || body.expires_at - body.sequence > MAX_INDEX_LIFETIME_SECONDS
    {
        return Err(IndexError::LifetimeTooLong);
    }
    if now >= body.expires_at {
        return Err(IndexError::Expired);
    }
    Ok(())
}

fn valid_url(url: &str) -> bool {
    !url.is_empty() && url.len() <= MAX_ENTRY_URL_BYTES && url.bytes().all(|b| b.is_ascii_graphic())
}

/// Structure of every entry. The core range is only length-bounded here: it
/// is parsed at install time, so one unusable range never voids the document.
fn check_entries(schema_version: u64, entries: &[IndexEntry]) -> Result<(), IndexError> {
    let mut versions = BTreeSet::new();
    let mut digests = BTreeSet::new();
    let mut runtime_owner: BTreeMap<&RuntimeId, &PackageId> = BTreeMap::new();
    let mut package_runtime: BTreeMap<&PackageId, &RuntimeId> = BTreeMap::new();
    for (index, entry) in entries.iter().enumerate() {
        let reject = |reason| IndexError::Entry { index, reason };
        if entry.platforms.is_empty()
            || entry.platforms.len() > MAX_PLATFORMS_PER_ENTRY
            || !entry.platforms.iter().all(|name| valid_platform(name))
            || entry.platforms.iter().collect::<BTreeSet<_>>().len() != entry.platforms.len()
        {
            return Err(reject(IndexEntryRejection::Platforms));
        }
        if !valid_url(&entry.url) {
            return Err(reject(IndexEntryRejection::Url));
        }
        if entry.core.is_empty() || entry.core.len() > MAX_CORE_RANGE_BYTES {
            return Err(reject(IndexEntryRejection::Core));
        }
        if !versions.insert((&entry.package_id, &entry.version)) {
            return Err(reject(IndexEntryRejection::DuplicateVersion));
        }
        if !digests.insert(&entry.digest) {
            return Err(reject(IndexEntryRejection::DuplicateDigest));
        }
        let kind_fits = matches!(
            (schema_version, entry.kind, &entry.runtime_id),
            (INDEX_SCHEMA_VERSION, None, Some(_))
                | (
                    INDEX_SCHEMA_VERSION_2,
                    Some(IndexEntryKind::Runtime),
                    Some(_)
                )
                | (INDEX_SCHEMA_VERSION_2, Some(IndexEntryKind::Context), None)
        );
        if !kind_fits {
            return Err(reject(IndexEntryRejection::Kind));
        }
        let id = entry.package_id.as_str();
        let id_fits = match entry.kind() {
            IndexEntryKind::Runtime => !id.starts_with(OFFICIAL_CONTEXT_PACKAGE_PREFIX),
            IndexEntryKind::Context => id
                .strip_prefix(OFFICIAL_CONTEXT_PACKAGE_PREFIX)
                .is_some_and(|tail| !tail.is_empty()),
        };
        if !id_fits {
            return Err(reject(IndexEntryRejection::PackageIdKind));
        }
        if let Some(runtime_id) = &entry.runtime_id {
            if runtime_owner
                .insert(runtime_id, &entry.package_id)
                .is_some_and(|owner| owner != &entry.package_id)
                || package_runtime
                    .insert(&entry.package_id, runtime_id)
                    .is_some_and(|runtime| runtime != runtime_id)
            {
                return Err(reject(IndexEntryRejection::IdentityConflict));
            }
        }
    }
    Ok(())
}

/// An index whose signature, revocations, freshness and entry structure were
/// verified.
#[derive(Clone, Debug)]
pub struct VerifiedIndex {
    sequence: u64,
    expires_at: u64,
    revocation_sequence: u64,
    signer: KeyId,
    revoked_key_ids: BTreeSet<KeyId>,
    entries: Vec<IndexEntry>,
}

impl VerifiedIndex {
    /// Publication sequence; the caller persists it as the next high-water
    /// mark.
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Unix second from which the index is no longer acceptable.
    #[must_use]
    pub fn expires_at(&self) -> u64 {
        self.expires_at
    }

    /// Sequence of the embedded revocation list; the caller persists it as the
    /// next revocation high-water mark.
    #[must_use]
    pub fn revocation_sequence(&self) -> u64 {
        self.revocation_sequence
    }

    /// Id of the index key whose signature was accepted.
    #[must_use]
    pub fn signer(&self) -> &KeyId {
        &self.signer
    }

    /// Key ids the embedded revocation list revokes; the caller accumulates
    /// them for later verifications.
    #[must_use]
    pub fn revoked_key_ids(&self) -> &BTreeSet<KeyId> {
        &self.revoked_key_ids
    }

    /// Entries in document order.
    #[must_use]
    pub fn entries(&self) -> &[IndexEntry] {
        &self.entries
    }

    /// Whether an entry has this archive digest.
    #[must_use]
    pub fn lists(&self, digest: &PackageDigest) -> bool {
        self.entries.iter().any(|entry| &entry.digest == digest)
    }

    /// Decides whether the index authorizes installing this runtime package on
    /// `core` and `platform`.
    ///
    /// Takes the entry whose digest equals `digest` and applies the checks
    /// below in order; the first failure wins. The entry must be of kind
    /// `runtime`, and the package id must be exactly
    /// [`OFFICIAL_RUNTIME_PACKAGE_PREFIX`] plus the runtime id, which keeps an
    /// entry from claiming a release package id. A pre-release core such as
    /// `0.41.0-rc.1` does not match `>=0.40.0`, so it gets
    /// [`IndexEntryRefusal::CoreExcluded`].
    ///
    /// # Errors
    ///
    /// Returns the [`IndexEntryRefusal`] of the first failed check.
    pub fn authorize(
        &self,
        package_id: &PackageId,
        runtime_id: &RuntimeId,
        version: &PackageVersion,
        digest: &PackageDigest,
        core: &Version,
        platform: &str,
    ) -> Result<(), IndexEntryRefusal> {
        let entry = self.entry_of(digest)?;
        if entry.kind() != IndexEntryKind::Runtime {
            return Err(IndexEntryRefusal::KindMismatch);
        }
        if package_id != &entry.package_id
            || entry.runtime_id.as_ref() != Some(runtime_id)
            || version != &entry.version
        {
            return Err(IndexEntryRefusal::IdentityMismatch);
        }
        if !package_id.as_str().starts_with(OFFICIAL_NAMESPACE) {
            return Err(IndexEntryRefusal::Namespace);
        }
        if INDEX_REFUSED_RUNTIME_IDS.contains(&runtime_id.as_str()) {
            return Err(IndexEntryRefusal::RuntimeRefused);
        }
        let bound = package_id
            .as_str()
            .strip_prefix(OFFICIAL_RUNTIME_PACKAGE_PREFIX)
            .is_some_and(|suffix| suffix == runtime_id.as_str());
        if !bound {
            return Err(IndexEntryRefusal::PackageIdBinding);
        }
        Self::check_support(entry, core, platform)
    }

    /// Decides whether the index authorizes installing this context package on
    /// `core` and `platform`.
    ///
    /// The entry whose digest equals `digest` must be of kind `context` and
    /// carry exactly this package id and version, and the id must be
    /// [`OFFICIAL_CONTEXT_PACKAGE_PREFIX`] plus a tail. A runtime entry never
    /// authorizes a context package.
    ///
    /// # Errors
    ///
    /// Returns the [`IndexEntryRefusal`] of the first failed check.
    pub fn authorize_context(
        &self,
        package_id: &PackageId,
        version: &PackageVersion,
        digest: &PackageDigest,
        core: &Version,
        platform: &str,
    ) -> Result<(), IndexEntryRefusal> {
        let entry = self.entry_of(digest)?;
        if entry.kind() != IndexEntryKind::Context {
            return Err(IndexEntryRefusal::KindMismatch);
        }
        if package_id != &entry.package_id || version != &entry.version {
            return Err(IndexEntryRefusal::IdentityMismatch);
        }
        if !package_id.as_str().starts_with(OFFICIAL_NAMESPACE) {
            return Err(IndexEntryRefusal::Namespace);
        }
        let bound = package_id
            .as_str()
            .strip_prefix(OFFICIAL_CONTEXT_PACKAGE_PREFIX)
            .is_some_and(|tail| !tail.is_empty());
        if !bound {
            return Err(IndexEntryRefusal::PackageIdBinding);
        }
        Self::check_support(entry, core, platform)
    }

    fn entry_of(&self, digest: &PackageDigest) -> Result<&IndexEntry, IndexEntryRefusal> {
        self.entries
            .iter()
            .find(|entry| &entry.digest == digest)
            .ok_or(IndexEntryRefusal::NotListed)
    }

    fn check_support(
        entry: &IndexEntry,
        core: &Version,
        platform: &str,
    ) -> Result<(), IndexEntryRefusal> {
        let range = parse_core_range(&entry.core).ok_or(IndexEntryRefusal::CoreRange)?;
        if !range.matches(core) {
            return Err(IndexEntryRefusal::CoreExcluded);
        }
        if !entry.platforms.iter().any(|listed| listed == platform) {
            return Err(IndexEntryRefusal::Platform);
        }
        Ok(())
    }
}
