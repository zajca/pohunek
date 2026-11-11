//! `index throwaway`: a self-contained plugin index trust for proofs.
//!
//! A random root and a random index key exist only while [`write`] runs. The
//! output holds an anchor that trusts only that root, a channel file naming it
//! and an index signed by the key, so a daemon started with the output
//! verifies the index while no release anchor does: the committed anchor and
//! channel never name the throwaway root. Neither seed is written anywhere.

// Rust guideline compliant 2026-11-06

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signer as _, SigningKey};
use package::{
    index_document_signing_message, index_key_record_signing_message, revocation_signing_message,
    verify_index, AnchorFile, CatalogSignature, IndexBody, IndexChannel, IndexEnvelope,
    IndexKeyRecord, IndexVerifierState, KeyId, RevocationBody, RootKey, SignedRevocations,
    INDEX_CHANNEL_FILE_NAME, INDEX_SCHEMA_VERSION, OFFICIAL_INDEX_FILE_NAME, OFFICIAL_SCOPE,
};
use zeroize::Zeroizing;

use super::build::entries_of;
use super::{document_bytes, write_new_document};
use crate::catalog::{hex, io_error, unix_now};
use crate::release::ANCHOR_FILE;
use crate::test_runtime::check_output_location;
use crate::XtaskError;

/// Bytes of operating-system randomness an Ed25519 seed takes.
const SEED_BYTES: usize = 32;

/// Seconds the root and the key are valid before the run: slack for a clock
/// that runs slightly behind the machine that verifies.
const VALID_BEFORE_SECONDS: u64 = 60;

/// Seconds the root, the key and the index stay valid after the run. A day
/// covers a proof run and leaves no long-lived document.
const VALID_AFTER_SECONDS: u64 = 24 * 60 * 60;

/// Sequence of the empty revocation list.
const REVOCATION_SEQUENCE: u64 = 1;

/// The inputs of `index throwaway`.
pub(crate) struct ThrowawayArgs {
    pub(crate) entries: PathBuf,
    pub(crate) output: PathBuf,
}

/// What `index throwaway` reports.
pub(crate) struct Written {
    pub(crate) entries: usize,
    pub(crate) endorser: KeyId,
    pub(crate) signer: KeyId,
}

/// A fresh Ed25519 key from operating-system randomness; the seed buffer is
/// cleared when it goes out of scope.
fn random_key() -> Result<SigningKey, XtaskError> {
    let mut seed = Zeroizing::new([0_u8; SEED_BYTES]);
    getrandom::getrandom(&mut *seed).map_err(|error| {
        XtaskError::Usage(format!(
            "operating-system randomness is unavailable: {error}"
        ))
    })?;
    Ok(SigningKey::from_bytes(&seed))
}

/// Refuses an output directory that is missing, not empty or inside the
/// checkout or a release tree.
fn check_output(output: &Path) -> Result<(), XtaskError> {
    let metadata = fs::symlink_metadata(output).map_err(io_error(output))?;
    if !metadata.is_dir() {
        return Err(XtaskError::Usage(format!(
            "the output `{}` is not an existing directory",
            output.display()
        )));
    }
    let mut listing = fs::read_dir(output).map_err(io_error(output))?;
    if listing.next().is_some() {
        return Err(XtaskError::Usage(format!(
            "the output `{}` is not empty",
            output.display()
        )));
    }
    check_output_location(&crate::repo_root(), output)
}

/// The three documents of a throwaway trust, as the bytes written to disk.
struct Documents {
    anchor: Vec<u8>,
    channel: Vec<u8>,
    index: Vec<u8>,
    entries: usize,
    endorser: KeyId,
    signer: KeyId,
}

fn assemble(args: &ThrowawayArgs, now: u64) -> Result<Documents, XtaskError> {
    let entries = entries_of(&args.entries, INDEX_SCHEMA_VERSION)?;
    let entry_count = entries.len();
    let not_before = now.saturating_sub(VALID_BEFORE_SECONDS);
    let not_after = now.saturating_add(VALID_AFTER_SECONDS);

    let root_key = random_key()?;
    let root_public = root_key.verifying_key();
    let root = RootKey::new(root_public.to_bytes(), not_before, not_after)
        .map_err(|error| XtaskError::Anchor(package::AnchorFileError::Anchor(error)))?;
    let anchor = AnchorFile::new(vec![root.clone()], Vec::new())
        .map_err(|error| XtaskError::Anchor(package::AnchorFileError::Anchor(error)))?
        .to_bytes()
        .map_err(XtaskError::Anchor)?;
    let channel = IndexChannel::new(root.id().clone())
        .to_bytes()
        .map_err(|error| XtaskError::Usage(error.to_string()))?;

    let index_key = random_key()?;
    let index_public = index_key.verifying_key();
    let mut record = IndexKeyRecord {
        key_id: KeyId::derive(&index_public),
        public_key: hex(&index_public.to_bytes()),
        scope: OFFICIAL_SCOPE.to_owned(),
        not_before,
        not_after,
        endorsed_by: root.id().clone(),
        signature: String::new(),
    };
    record.signature = hex(&root_key
        .sign(&index_key_record_signing_message(&record))
        .to_bytes());

    let body = RevocationBody {
        schema_version: INDEX_SCHEMA_VERSION,
        sequence: REVOCATION_SEQUENCE,
        revoked_key_ids: Vec::new(),
    };
    let revocation_signature = root_key.sign(
        &revocation_signing_message(&body).map_err(|error| XtaskError::Usage(error.to_string()))?,
    );
    let revocations = SignedRevocations {
        revocations: body,
        signature: CatalogSignature {
            key_id: root.id().clone(),
            signature: hex(&revocation_signature.to_bytes()),
        },
    };

    let signer = record.key_id.clone();
    let mut envelope = IndexEnvelope {
        schema_version: INDEX_SCHEMA_VERSION,
        index: IndexBody {
            schema_version: INDEX_SCHEMA_VERSION,
            scope: OFFICIAL_SCOPE.to_owned(),
            sequence: now,
            expires_at: not_after,
            entries,
        },
        key_records: vec![record],
        signatures: Vec::new(),
        revocations,
    };
    let message =
        index_document_signing_message(&document_bytes(&envelope)?).map_err(XtaskError::Index)?;
    envelope.signatures.push(CatalogSignature {
        key_id: signer.clone(),
        signature: hex(&index_key.sign(&message).to_bytes()),
    });
    let index = document_bytes(&envelope)?;

    let verified = verify_index(
        &index,
        &root,
        &BTreeSet::new(),
        &IndexVerifierState::default(),
        now,
    )
    .map_err(XtaskError::Index)?;
    if verified.signer() != &signer {
        return Err(XtaskError::Usage(
            "the throwaway index verified under another signer".into(),
        ));
    }
    Ok(Documents {
        anchor,
        channel,
        index,
        entries: entry_count,
        endorser: root.id().clone(),
        signer,
    })
}

/// Writes the anchor, the channel and the signed index into `args.output`.
///
/// The output must be an existing empty directory outside the checkout. The
/// index is verified under the root the anchor holds before anything is written,
/// and a failed write removes the files this call created.
pub(crate) fn write(args: &ThrowawayArgs) -> Result<Written, XtaskError> {
    check_output(&args.output)?;
    let documents = assemble(args, unix_now()?)?;
    let files = [
        (ANCHOR_FILE, &documents.anchor),
        (INDEX_CHANNEL_FILE_NAME, &documents.channel),
        (OFFICIAL_INDEX_FILE_NAME, &documents.index),
    ];
    let mut created: Vec<PathBuf> = Vec::with_capacity(files.len());
    for (name, bytes) in files {
        let path = args.output.join(name);
        if let Err(error) = write_new_document(&path, bytes) {
            // The directory was empty, so every file in `created` is ours.
            for file in &created {
                let _ = fs::remove_file(file);
            }
            return Err(error);
        }
        created.push(path);
    }
    Ok(Written {
        entries: documents.entries,
        endorser: documents.endorser,
        signer: documents.signer,
    })
}
