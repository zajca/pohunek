//! `index message`, `index attach` and `index verify`: the offline half of
//! signing a plugin index.
//!
//! No key reaches this module. `message` writes the bytes a signature covers,
//! the signing step runs outside the workspace's code, and `attach` inserts
//! the externally produced signature after the daemon's own verifier accepted
//! the result. `verify` runs the same verifier, and the material form checks a
//! key record or a revocation list with the package crate's per-document rules.

// Rust guideline compliant 2026-11-06

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use package::{
    index_document_signing_message, parse_anchor, parse_index_channel, parse_index_envelope,
    verify_index, verify_key_record_document, verify_revocation_document, CatalogSignature,
    IndexVerifierState, KeyId, RootKey, MAX_ANCHOR_BYTES, MAX_INDEX_BYTES, MAX_INDEX_CHANNEL_BYTES,
};
use sha2::{Digest as _, Sha256};

use super::{
    check_successor, document_bytes, hex_array, parse_key_id, write_new_document,
    MAX_REVOCATIONS_FILE_BYTES,
};
use crate::catalog::{hex, read_input};
use crate::XtaskError;

/// Hex characters of an Ed25519 signature.
const SIGNATURE_HEX_LEN: usize = 128;

/// Largest signature file read: the hex plus a newline, with slack so a longer
/// file is reported as malformed instead of as oversized.
const MAX_SIGNATURE_FILE_BYTES: u64 = 256;

/// The endorser root and the revoked key ids a verification starts from.
struct Trust {
    endorser: RootKey,
    base_revoked: BTreeSet<KeyId>,
}

/// Resolves the index endorser from the channel file and the anchor, the way
/// the daemon does, and collects the anchor's revoked key ids.
fn load_trust(anchor: &Path, channel: &Path) -> Result<Trust, XtaskError> {
    let anchor_bytes = read_input(anchor, u64::try_from(MAX_ANCHOR_BYTES).unwrap_or(u64::MAX))?;
    let anchor = parse_anchor(&anchor_bytes).map_err(XtaskError::Anchor)?;
    let channel_bytes = read_input(
        channel,
        u64::try_from(MAX_INDEX_CHANNEL_BYTES).unwrap_or(u64::MAX),
    )?;
    let channel = parse_index_channel(&channel_bytes)
        .map_err(|error| XtaskError::Usage(format!("plugin index channel file: {error}")))?;
    let endorser = channel
        .endorser(&anchor)
        .map_err(|error| XtaskError::Usage(format!("plugin index channel file: {error}")))?;
    Ok(Trust {
        endorser,
        base_revoked: anchor.revoked_key_ids().iter().cloned().collect(),
    })
}

fn read_index(path: &Path) -> Result<Vec<u8>, XtaskError> {
    read_input(path, u64::try_from(MAX_INDEX_BYTES).unwrap_or(u64::MAX))
}

/// What `index message` reports.
pub(crate) struct MessageWritten {
    pub(crate) sha256: String,
    pub(crate) bytes: usize,
}

/// Writes the message an index signature covers for the unsigned document.
pub(crate) fn message(unsigned: &Path, output: &Path) -> Result<MessageWritten, XtaskError> {
    let document = read_index(unsigned)?;
    let message = index_document_signing_message(&document).map_err(XtaskError::Index)?;
    write_new_document(output, &message)?;
    Ok(MessageWritten {
        sha256: hex(&Sha256::digest(&message)),
        bytes: message.len(),
    })
}

/// The inputs of `index attach`.
pub(crate) struct AttachArgs {
    pub(crate) unsigned: PathBuf,
    pub(crate) key_id: String,
    pub(crate) signature: PathBuf,
    pub(crate) anchor: PathBuf,
    pub(crate) channel: PathBuf,
    pub(crate) output: PathBuf,
}

/// Reads a signature file: exactly 128 lowercase hex characters and an
/// optional final newline.
fn read_signature(path: &Path) -> Result<String, XtaskError> {
    let bytes = read_input(path, MAX_SIGNATURE_FILE_BYTES)?;
    let text = std::str::from_utf8(&bytes).map_err(|_cause| malformed_signature(path))?;
    let hex_text = text.strip_suffix('\n').unwrap_or(text);
    if hex_text.len() != SIGNATURE_HEX_LEN || hex_array::<64>(hex_text).is_none() {
        return Err(malformed_signature(path));
    }
    Ok(hex_text.to_owned())
}

fn malformed_signature(path: &Path) -> XtaskError {
    XtaskError::Usage(format!(
        "`{}` is not 128 lowercase hex characters with an optional final newline",
        path.display()
    ))
}

/// Inserts the external signature of `args.key_id` into the unsigned document
/// and writes the signed document.
///
/// The signed bytes are verified with the daemon's verifier, the endorser
/// resolved from the channel and the anchor, no persisted marks and the clock
/// `now`, and only a document whose signer is `args.key_id` is written.
pub(crate) fn attach(args: &AttachArgs, now: u64) -> Result<(), XtaskError> {
    let key_id = parse_key_id(&args.key_id, "--key-id")?;
    let trust = load_trust(&args.anchor, &args.channel)?;
    let mut envelope =
        parse_index_envelope(&read_index(&args.unsigned)?).map_err(XtaskError::Index)?;
    if !envelope.signatures.is_empty() {
        return Err(XtaskError::Usage(
            "the document already carries signatures; attach needs an unsigned one".into(),
        ));
    }
    if !envelope
        .key_records
        .iter()
        .any(|record| record.key_id == key_id)
    {
        return Err(XtaskError::Usage(format!(
            "no key record of the document has the key id {key_id}"
        )));
    }
    envelope.signatures.push(CatalogSignature {
        key_id: key_id.clone(),
        signature: read_signature(&args.signature)?,
    });

    let signed = document_bytes(&envelope)?;
    let verified = verify_index(
        &signed,
        &trust.endorser,
        &trust.base_revoked,
        &IndexVerifierState::default(),
        now,
    )
    .map_err(XtaskError::Index)?;
    if verified.signer() != &key_id {
        return Err(XtaskError::Usage(format!(
            "the document verified under {} instead of {key_id}",
            verified.signer()
        )));
    }
    write_new_document(&args.output, &signed)
}

/// The inputs of `index verify` for a whole index.
pub(crate) struct VerifyIndexArgs {
    pub(crate) index: PathBuf,
    pub(crate) anchor: PathBuf,
    pub(crate) channel: PathBuf,
    pub(crate) index_high_water: Option<u64>,
    pub(crate) revocation_high_water: Option<u64>,
}

/// What `index verify` reports for an index.
pub(crate) struct IndexVerified {
    pub(crate) sequence: u64,
    pub(crate) expires_at: u64,
    pub(crate) signer: KeyId,
    pub(crate) entries: usize,
}

/// Verifies the index at `args.index` as the daemon would at `now`.
pub(crate) fn verify_whole(args: &VerifyIndexArgs, now: u64) -> Result<IndexVerified, XtaskError> {
    let trust = load_trust(&args.anchor, &args.channel)?;
    let state = IndexVerifierState {
        index_high_water: args.index_high_water,
        revocation_high_water: args.revocation_high_water,
        revoked_key_ids: BTreeSet::new(),
    };
    let verified = verify_index(
        &read_index(&args.index)?,
        &trust.endorser,
        &trust.base_revoked,
        &state,
        now,
    )
    .map_err(XtaskError::Index)?;
    Ok(IndexVerified {
        sequence: verified.sequence(),
        expires_at: verified.expires_at(),
        signer: verified.signer().clone(),
        entries: verified.entries().len(),
    })
}

/// The inputs of `index verify` for key material.
pub(crate) struct VerifyMaterialArgs {
    pub(crate) record: PathBuf,
    pub(crate) revocations: PathBuf,
    pub(crate) previous_revocations: Option<PathBuf>,
    pub(crate) anchor: PathBuf,
    pub(crate) channel: PathBuf,
}

/// What `index verify` reports for key material.
pub(crate) struct MaterialVerified {
    pub(crate) key_id: KeyId,
    pub(crate) revocation_sequence: u64,
}

/// Verifies a key record and a revocation list under the channel's endorser.
///
/// With `previous_revocations` the new list must also have a strictly greater
/// sequence and keep every previously revoked id.
pub(crate) fn verify_material(args: &VerifyMaterialArgs) -> Result<MaterialVerified, XtaskError> {
    let trust = load_trust(&args.anchor, &args.channel)?;
    let record = verify_key_record_document(
        &read_input(
            &args.record,
            u64::try_from(MAX_INDEX_BYTES).unwrap_or(u64::MAX),
        )?,
        &trust.endorser,
    )
    .map_err(XtaskError::Index)?;
    let revocations = verify_revocation_document(
        &read_input(&args.revocations, MAX_REVOCATIONS_FILE_BYTES)?,
        &trust.endorser,
    )
    .map_err(XtaskError::Index)?;
    // The endorser and the record's key must be live in the daemon's revoked
    // set: the anchor's ids plus the list being checked.
    let revoked: BTreeSet<&KeyId> = trust
        .base_revoked
        .iter()
        .chain(revocations.revocations.revoked_key_ids.iter())
        .collect();
    if revoked.contains(trust.endorser.id()) || revoked.contains(&record.key_id) {
        return Err(XtaskError::Index(package::IndexError::SignerRevoked));
    }
    if let Some(path) = &args.previous_revocations {
        let previous = verify_revocation_document(
            &read_input(path, MAX_REVOCATIONS_FILE_BYTES)?,
            &trust.endorser,
        )
        .map_err(XtaskError::Index)?;
        let revoked: BTreeSet<KeyId> = revocations
            .revocations
            .revoked_key_ids
            .iter()
            .cloned()
            .collect();
        check_successor(
            &previous.revocations,
            revocations.revocations.sequence,
            &revoked,
        )?;
    }
    Ok(MaterialVerified {
        key_id: record.key_id,
        revocation_sequence: revocations.revocations.sequence,
    })
}
