//! The catalog trust anchor file shipped with a release.
//!
//! A host trusts official packages only through the root keys of this file,
//! which sits beside the daemon executable (see [`ANCHOR_FILE_NAME`]). The
//! file holds public keys, validity windows and revoked key ids and nothing
//! secret. Key ids are derived from the public keys, so each root's `key_id`
//! member is a cross-check: a file whose id does not match its key is
//! rejected instead of trusted under a name the key does not have.
//!
//! The document is parsed with the same strict JSON subset as the catalog
//! (no duplicate keys, floats or trailing data), bounded by
//! [`MAX_ANCHOR_BYTES`] before any parsing. Errors are typed and never carry
//! file content. Reading the file from disk and judging its ownership and
//! permissions belong to the caller; this module is pure.
//!
//! The format is described in `docs/knowledge/concepts/runtime-catalog.md`.

// Rust guideline compliant 2026-10-05

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::canonical_json::{parse_strict, JsonError};
use crate::catalog::{decode_hex, encode_hex};
use crate::{KeyId, RootKey, TrustAnchor, TrustAnchorError};

/// File name of the trust anchor in the release bundle.
pub const ANCHOR_FILE_NAME: &str = "runtime-catalog-anchor.json";

/// Anchor schema version this build reads and writes.
pub const ANCHOR_SCHEMA_VERSION: u64 = 1;

/// Largest accepted anchor file: 16 KiB.
///
/// The format admits at most eight roots of about 200 bytes and 64 revoked
/// key ids of about 70 bytes, so a legitimate file stays under 7 KiB. The
/// ceiling bounds what is parsed before the anchor is trusted.
pub const MAX_ANCHOR_BYTES: usize = 16 * 1024;

/// Length of an Ed25519 public key in bytes.
const PUBLIC_KEY_BYTES: usize = 32;

/// One trusted root as the anchor file spells it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct AnchorRootRecord {
    /// Id derived from `public_key`.
    key_id: KeyId,
    /// 64 lowercase hex characters of the raw Ed25519 public key.
    public_key: String,
    /// First Unix second the root may sign.
    not_before: u64,
    /// Unix second from which the root may no longer sign.
    not_after: u64,
}

/// The anchor file document.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct AnchorDocument {
    schema_version: u64,
    roots: Vec<AnchorRootRecord>,
    revoked_key_ids: Vec<KeyId>,
}

/// Why an anchor file was rejected.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AnchorFileError {
    /// The input exceeds [`MAX_ANCHOR_BYTES`].
    #[error("trust anchor file exceeds the size limit")]
    TooLarge,
    /// The input is not one well-formed JSON value.
    #[error("trust anchor file is not well-formed JSON")]
    Syntax,
    /// An object repeats a key.
    #[error("trust anchor file repeats a JSON object key")]
    DuplicateKey,
    /// A number is negative, fractional or in exponent form.
    #[error("trust anchor file uses a number outside the non-negative integer subset")]
    UnsupportedNumber,
    /// The document does not match the anchor schema.
    #[error("trust anchor file does not match the schema")]
    Schema,
    /// The schema version is not supported.
    #[error("unsupported trust anchor schema version")]
    UnsupportedSchemaVersion,
    /// A root's public key is not 64 lowercase hex characters.
    #[error("trust anchor file contains a malformed public key")]
    MalformedKey,
    /// A root's `key_id` does not match its public key.
    #[error("trust anchor root id does not match its public key")]
    KeyIdMismatch,
    /// The roots and revocations do not form a valid anchor.
    #[error("trust anchor is invalid: {0}")]
    Anchor(#[from] TrustAnchorError),
}

impl From<JsonError> for AnchorFileError {
    fn from(error: JsonError) -> Self {
        match error {
            JsonError::Syntax => Self::Syntax,
            JsonError::DuplicateKey => Self::DuplicateKey,
            JsonError::UnsupportedNumber => Self::UnsupportedNumber,
        }
    }
}

/// A validated trust anchor: root keys with their windows and the key ids
/// revoked so far.
#[derive(Clone, Debug)]
pub struct AnchorFile {
    roots: Vec<RootKey>,
    revoked: Vec<KeyId>,
}

impl AnchorFile {
    /// Validates `roots` and `revoked` as an anchor.
    ///
    /// # Errors
    ///
    /// Returns the [`TrustAnchorError`] of [`TrustAnchor::new`] for no roots,
    /// too many roots or revoked ids, or a repeated root.
    pub fn new(roots: Vec<RootKey>, revoked: Vec<KeyId>) -> Result<Self, TrustAnchorError> {
        TrustAnchor::new(roots.clone(), revoked.clone())?;
        Ok(Self { roots, revoked })
    }

    /// The root keys.
    #[must_use]
    pub fn roots(&self) -> &[RootKey] {
        &self.roots
    }

    /// The revoked key ids.
    #[must_use]
    pub fn revoked_key_ids(&self) -> &[KeyId] {
        &self.revoked
    }

    /// Splits the anchor into its root keys and revoked key ids.
    #[must_use]
    pub fn into_parts(self) -> (Vec<RootKey>, Vec<KeyId>) {
        (self.roots, self.revoked)
    }

    /// The verifier input for this anchor.
    ///
    /// # Errors
    ///
    /// Returns the [`TrustAnchorError`] of [`TrustAnchor::new`]; an anchor
    /// built by [`AnchorFile::new`] or [`parse_anchor`] never fails here.
    pub fn trust_anchor(&self) -> Result<TrustAnchor, TrustAnchorError> {
        TrustAnchor::new(self.roots.clone(), self.revoked.clone())
    }

    /// The deterministic document bytes: roots and revoked ids in ascending id
    /// order, indented, with one trailing newline.
    ///
    /// # Errors
    ///
    /// Returns [`AnchorFileError::Schema`] if the document cannot be
    /// serialized.
    pub fn to_bytes(&self) -> Result<Vec<u8>, AnchorFileError> {
        let mut roots: Vec<&RootKey> = self.roots.iter().collect();
        roots.sort_by(|left, right| left.id().cmp(right.id()));
        let mut revoked = self.revoked.clone();
        revoked.sort();
        let document = AnchorDocument {
            schema_version: ANCHOR_SCHEMA_VERSION,
            roots: roots
                .into_iter()
                .map(|root| AnchorRootRecord {
                    key_id: root.id().clone(),
                    public_key: encode_hex(&root.public_key()),
                    not_before: root.not_before(),
                    not_after: root.not_after(),
                })
                .collect(),
            revoked_key_ids: revoked,
        };
        let mut bytes =
            serde_json::to_vec_pretty(&document).map_err(|_cause| AnchorFileError::Schema)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

/// Parses and validates an anchor file.
///
/// # Errors
///
/// Returns the first violated rule as an [`AnchorFileError`].
pub fn parse_anchor(bytes: &[u8]) -> Result<AnchorFile, AnchorFileError> {
    if bytes.len() > MAX_ANCHOR_BYTES {
        return Err(AnchorFileError::TooLarge);
    }
    let value = parse_strict(bytes)?;
    let document: AnchorDocument =
        serde_json::from_value(value).map_err(|_cause| AnchorFileError::Schema)?;
    if document.schema_version != ANCHOR_SCHEMA_VERSION {
        return Err(AnchorFileError::UnsupportedSchemaVersion);
    }
    let mut roots = Vec::with_capacity(document.roots.len());
    for record in &document.roots {
        let public_key = decode_hex::<PUBLIC_KEY_BYTES>(&record.public_key)
            .ok_or(AnchorFileError::MalformedKey)?;
        let root = RootKey::new(public_key, record.not_before, record.not_after)?;
        if root.id() != &record.key_id {
            return Err(AnchorFileError::KeyIdMismatch);
        }
        roots.push(root);
    }
    Ok(AnchorFile::new(roots, document.revoked_key_ids)?)
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::SigningKey;
    use serde_json::{json, Value};

    use super::*;

    const WINDOW_START: u64 = 100;
    const WINDOW_END: u64 = 200;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn root(seed: u8) -> RootKey {
        RootKey::new(
            key(seed).verifying_key().to_bytes(),
            WINDOW_START,
            WINDOW_END,
        )
        .expect("root")
    }

    fn document(seeds: &[u8]) -> Value {
        let anchor = AnchorFile::new(seeds.iter().map(|seed| root(*seed)).collect(), Vec::new())
            .expect("anchor");
        serde_json::from_slice(&anchor.to_bytes().expect("bytes")).expect("json")
    }

    fn parse_value(value: &Value) -> Result<AnchorFile, AnchorFileError> {
        parse_anchor(&serde_json::to_vec(value).expect("serialize"))
    }

    #[test]
    fn a_written_anchor_reads_back_with_its_roots_and_revocations() {
        let revoked = KeyId::derive(&key(9).verifying_key());
        let written = AnchorFile::new(vec![root(2), root(1)], vec![revoked.clone()]).expect("new");
        let read = parse_anchor(&written.to_bytes().expect("bytes")).expect("parse");
        let mut expected: Vec<KeyId> = [root(1), root(2)]
            .iter()
            .map(|root| root.id().clone())
            .collect();
        expected.sort();
        let actual: Vec<KeyId> = read.roots().iter().map(|root| root.id().clone()).collect();
        assert_eq!(actual, expected, "roots come back in ascending id order");
        assert_eq!(read.roots()[0].not_before(), WINDOW_START);
        assert_eq!(read.roots()[0].not_after(), WINDOW_END);
        assert_eq!(read.revoked_key_ids(), [revoked]);
        read.trust_anchor().expect("verifier input");
    }

    #[test]
    fn the_bytes_do_not_depend_on_the_order_the_roots_were_given_in() {
        let first = AnchorFile::new(vec![root(1), root(2)], Vec::new()).expect("new");
        let second = AnchorFile::new(vec![root(2), root(1)], Vec::new()).expect("new");
        assert_eq!(
            first.to_bytes().expect("bytes"),
            second.to_bytes().expect("bytes")
        );
        assert!(first.to_bytes().expect("bytes").ends_with(b"}\n"));
    }

    #[test]
    fn a_key_id_that_does_not_match_the_public_key_is_refused() {
        let mut value = document(&[1]);
        value["roots"][0]["key_id"] = json!(KeyId::derive(&key(2).verifying_key()).as_str());
        assert_eq!(
            parse_value(&value).expect_err("mismatch"),
            AnchorFileError::KeyIdMismatch
        );
    }

    #[test]
    fn malformed_public_keys_are_refused() {
        for bad in ["", "zz", &"0".repeat(63), &"A".repeat(64), &"0".repeat(64)] {
            let mut value = document(&[1]);
            value["roots"][0]["public_key"] = json!(bad);
            let error = parse_value(&value).expect_err("malformed key");
            assert!(
                matches!(
                    error,
                    AnchorFileError::MalformedKey
                        | AnchorFileError::Anchor(TrustAnchorError::MalformedKey)
                ),
                "{bad:?} gave {error:?}"
            );
        }
    }

    #[test]
    fn an_empty_window_and_an_empty_root_list_are_refused() {
        let mut value = document(&[1]);
        value["roots"][0]["not_after"] = json!(WINDOW_START);
        assert_eq!(
            parse_value(&value).expect_err("empty window"),
            AnchorFileError::Anchor(TrustAnchorError::EmptyWindow)
        );
        assert_eq!(
            parse_value(&json!({"schema_version": 1, "roots": [], "revoked_key_ids": []}))
                .expect_err("no roots"),
            AnchorFileError::Anchor(TrustAnchorError::NoRoots)
        );
    }

    #[test]
    fn a_repeated_root_is_refused() {
        let mut value = document(&[1]);
        let record = value["roots"][0].clone();
        value["roots"].as_array_mut().expect("roots").push(record);
        assert_eq!(
            parse_value(&value).expect_err("duplicate"),
            AnchorFileError::Anchor(TrustAnchorError::DuplicateRoot)
        );
    }

    #[test]
    fn unknown_members_and_unsupported_versions_are_refused() {
        let mut value = document(&[1]);
        value["extra"] = json!(1);
        assert_eq!(
            parse_value(&value).expect_err("unknown member"),
            AnchorFileError::Schema
        );
        let mut value = document(&[1]);
        value["roots"][0]["extra"] = json!(1);
        assert_eq!(
            parse_value(&value).expect_err("unknown root member"),
            AnchorFileError::Schema
        );
        let mut value = document(&[1]);
        value["schema_version"] = json!(2);
        assert_eq!(
            parse_value(&value).expect_err("version"),
            AnchorFileError::UnsupportedSchemaVersion
        );
    }

    #[test]
    fn the_strict_json_subset_and_the_size_bound_apply() {
        assert_eq!(
            parse_anchor(b"").expect_err("empty"),
            AnchorFileError::Syntax
        );
        assert_eq!(
            parse_anchor(b"{").expect_err("truncated"),
            AnchorFileError::Syntax
        );
        let good = document(&[1]).to_string();
        assert_eq!(
            parse_anchor(format!("{good} trailing").as_bytes()).expect_err("trailing"),
            AnchorFileError::Syntax
        );
        assert_eq!(
            parse_anchor(
                br#"{"schema_version":1,"schema_version":1,"roots":[],"revoked_key_ids":[]}"#
            )
            .expect_err("duplicate key"),
            AnchorFileError::DuplicateKey
        );
        assert_eq!(
            parse_anchor(br#"{"schema_version":1.0,"roots":[],"revoked_key_ids":[]}"#)
                .expect_err("float"),
            AnchorFileError::UnsupportedNumber
        );
        assert_eq!(
            parse_anchor(&vec![b' '; MAX_ANCHOR_BYTES + 1]).expect_err("too large"),
            AnchorFileError::TooLarge
        );
    }

    #[test]
    fn errors_never_carry_file_content() {
        let mut value = document(&[1]);
        value["roots"][0]["public_key"] = json!("secret-looking-text");
        let message = parse_value(&value).expect_err("malformed").to_string();
        assert!(!message.contains("secret-looking-text"), "{message}");
    }
}
