//! The plugin index channel file shipped with a release.
//!
//! A host verifies plugin indexes against one designated endorser root. The
//! release bundle names that root in `plugin-index-channel.json`, beside the
//! trust anchor, by key id; the root itself comes from the anchor, so the
//! channel file carries no key material. [`IndexChannel::endorser`] resolves
//! the id against the anchor and fails when the anchor does not hold that
//! root.
//!
//! The document is parsed with the strict JSON subset of the catalog (no
//! duplicate keys, floats or trailing data) and bounded by
//! [`MAX_INDEX_CHANNEL_BYTES`] before any parsing. Errors are typed and never
//! carry file content. Reading the file from disk and judging its ownership
//! and permissions belong to the caller, which treats a missing file as a
//! refusal; this module is pure.
//!
//! The format is described in `docs/knowledge/concepts/runtime-catalog.md`.

// Rust guideline compliant 2026-10-05

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::anchor::AnchorFile;
use crate::canonical_json::{parse_strict, JsonError};
use crate::catalog::{KeyId, RootKey};

/// File name of the channel document in the release bundle.
pub const INDEX_CHANNEL_FILE_NAME: &str = "plugin-index-channel.json";

/// Channel schema version this build reads and writes.
pub const INDEX_CHANNEL_SCHEMA_VERSION: u64 = 1;

/// Largest accepted channel file: 4 KiB.
///
/// The document holds a schema number and one 64-character key id, about
/// 100 bytes. The ceiling bounds what is parsed before the file is trusted.
pub const MAX_INDEX_CHANNEL_BYTES: usize = 4096;

/// The channel document.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ChannelDocument {
    schema_version: u64,
    index_endorser_key_id: KeyId,
}

/// Why a channel file was rejected or could not be resolved.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum IndexChannelError {
    /// The input exceeds [`MAX_INDEX_CHANNEL_BYTES`].
    #[error("plugin index channel file exceeds the size limit")]
    TooLarge,
    /// The input is not one well-formed JSON value.
    #[error("plugin index channel file is not well-formed JSON")]
    Syntax,
    /// An object repeats a key.
    #[error("plugin index channel file repeats a JSON object key")]
    DuplicateKey,
    /// A number is negative, fractional or in exponent form.
    #[error("plugin index channel file uses a number outside the non-negative integer subset")]
    UnsupportedNumber,
    /// The document does not match the channel schema.
    #[error("plugin index channel file does not match the schema")]
    Schema,
    /// The schema version is not supported.
    #[error("unsupported plugin index channel schema version")]
    UnsupportedSchemaVersion,
    /// The trust anchor holds no root with the channel's endorser id.
    #[error("plugin index endorser is not a root of the trust anchor")]
    EndorserNotInAnchor,
}

impl From<JsonError> for IndexChannelError {
    fn from(error: JsonError) -> Self {
        match error {
            JsonError::Syntax => Self::Syntax,
            JsonError::DuplicateKey => Self::DuplicateKey,
            JsonError::UnsupportedNumber => Self::UnsupportedNumber,
        }
    }
}

/// A validated channel document: the key id of the plugin index endorser.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexChannel {
    index_endorser_key_id: KeyId,
}

impl IndexChannel {
    /// A channel naming `index_endorser_key_id` as the index endorser.
    #[must_use]
    pub fn new(index_endorser_key_id: KeyId) -> Self {
        Self {
            index_endorser_key_id,
        }
    }

    /// The key id of the designated index endorser root.
    #[must_use]
    pub fn index_endorser_key_id(&self) -> &KeyId {
        &self.index_endorser_key_id
    }

    /// The root of `anchor` whose id equals the channel's endorser id.
    ///
    /// # Errors
    ///
    /// Returns [`IndexChannelError::EndorserNotInAnchor`] when no root of the
    /// anchor has that id.
    pub fn endorser(&self, anchor: &AnchorFile) -> Result<RootKey, IndexChannelError> {
        anchor
            .roots()
            .iter()
            .find(|root| root.id() == &self.index_endorser_key_id)
            .cloned()
            .ok_or(IndexChannelError::EndorserNotInAnchor)
    }

    /// The deterministic document bytes: indented, with one trailing newline.
    ///
    /// # Errors
    ///
    /// Returns [`IndexChannelError::Schema`] if the document cannot be
    /// serialized.
    pub fn to_bytes(&self) -> Result<Vec<u8>, IndexChannelError> {
        let document = ChannelDocument {
            schema_version: INDEX_CHANNEL_SCHEMA_VERSION,
            index_endorser_key_id: self.index_endorser_key_id.clone(),
        };
        let mut bytes =
            serde_json::to_vec_pretty(&document).map_err(|_cause| IndexChannelError::Schema)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

/// Parses and validates a channel file.
///
/// # Errors
///
/// Returns the first violated rule as an [`IndexChannelError`].
pub fn parse_index_channel(bytes: &[u8]) -> Result<IndexChannel, IndexChannelError> {
    if bytes.len() > MAX_INDEX_CHANNEL_BYTES {
        return Err(IndexChannelError::TooLarge);
    }
    let value = parse_strict(bytes)?;
    let document: ChannelDocument =
        serde_json::from_value(value).map_err(|_cause| IndexChannelError::Schema)?;
    if document.schema_version != INDEX_CHANNEL_SCHEMA_VERSION {
        return Err(IndexChannelError::UnsupportedSchemaVersion);
    }
    Ok(IndexChannel::new(document.index_endorser_key_id))
}
