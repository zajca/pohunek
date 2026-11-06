//! Plugin index commands: the owner ceremony (`catalog keygen`,
//! `index endorse`, `index revoke`, `index channel`) and the index tooling
//! (`index build`, `message`, `attach`, `verify`, `throwaway`).
//!
//! The commands run on the owner's machine in the key ceremony. A seed is read
//! only from a key file under the rules of [`crate::catalog_key`] and is never
//! printed, logged or taken from an argument. Every document is verified with
//! the rules the daemon's index verifier applies before it is written, and is
//! written to a path that must not exist yet, so a refused input leaves no
//! file and a finished document is never overwritten. The index tooling never
//! reads a key file: signing happens outside the process.

// Rust guideline compliant 2026-11-06

mod admit;
mod build;
mod sign;
mod throwaway;

pub(crate) use admit::{check_entry, CheckArgs, CheckStage};
pub(crate) use build::{build, entry_of, print_source, BuildArgs};
pub(crate) use sign::{
    attach, message, verify_material, verify_whole, AttachArgs, VerifyIndexArgs, VerifyMaterialArgs,
};
pub(crate) use throwaway::{write as throwaway, ThrowawayArgs};

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use nix::fcntl::OFlag;
use package::{
    index_key_record_signing_message, parse_anchor, revocation_signing_message, CatalogSignature,
    IndexChannel, IndexKeyRecord, KeyId, RevocationBody, SignedRevocations, INDEX_SCHEMA_VERSION,
    MAX_ANCHOR_BYTES, MAX_INDEX_KEY_WINDOW_SECONDS, MAX_INDEX_LIFETIME_SECONDS, MAX_REVOKED_KEYS,
    OFFICIAL_SCOPE,
};
use zeroize::Zeroizing;

use crate::catalog::{hex, io_error, read_input};
use crate::catalog_key::{read_signing_key, write_new_key_file};
use crate::XtaskError;

/// Mode of every public document the ceremony writes: readable by all.
const DOCUMENT_MODE: u32 = 0o644;

/// Bytes of operating-system randomness an Ed25519 seed takes.
const SEED_BYTES: usize = 32;

/// Largest accepted `--previous` revocation document.
///
/// A document with [`MAX_REVOKED_KEYS`] ids of 64 characters each is below
/// 8 KiB when indented; 16 KiB leaves room for formatting without admitting an
/// arbitrary file.
const MAX_REVOCATIONS_FILE_BYTES: u64 = 16 * 1024;

/// The sequence of the first revocation document of a chain.
const FIRST_REVOCATION_SEQUENCE: u64 = 1;

/// Parses a 64-character lowercase hex key id given on the command line.
fn parse_key_id(text: &str, what: &str) -> Result<KeyId, XtaskError> {
    KeyId::parse(text)
        .map_err(|_cause| XtaskError::Usage(format!("{what} is not 64 lowercase hex characters")))
}

/// Writes `bytes` to the new file `path` with mode 0644, without following a
/// final symbolic link.
///
/// The create is exclusive, so an existing path is refused and never changed.
/// A failure after the file was created removes it again.
pub(crate) fn write_new_document(path: &Path, bytes: &[u8]) -> Result<(), XtaskError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(DOCUMENT_MODE)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC).bits())
        .open(path)
        .map_err(io_error(path))?;
    // The create mode is masked by the umask; the document is public, so the
    // mode is set explicitly.
    let written = file
        .set_permissions(std::fs::Permissions::from_mode(DOCUMENT_MODE))
        .and_then(|()| file.write_all(bytes))
        .and_then(|()| file.sync_all());
    if let Err(source) = written {
        // The file is ours: it did not exist before the exclusive create.
        let _ = std::fs::remove_file(path);
        return Err(XtaskError::Io {
            path: path.to_path_buf(),
            source,
        });
    }
    Ok(())
}

/// The document bytes: indented JSON with one trailing newline.
pub(crate) fn document_bytes<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, XtaskError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(XtaskError::Json)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// What `catalog keygen` reports: the new key's id and public key.
pub(crate) struct NewKey {
    pub(crate) key_id: KeyId,
    pub(crate) public_key: String,
}

/// Creates a new Ed25519 key file at `output` and returns its public half.
///
/// The seed comes from operating-system randomness, lives in a zeroizing
/// buffer and leaves the process only through the 0600 key file.
pub(crate) fn keygen(output: &Path) -> Result<NewKey, XtaskError> {
    let mut seed = Zeroizing::new([0_u8; SEED_BYTES]);
    getrandom::getrandom(&mut *seed).map_err(|error| {
        XtaskError::Usage(format!(
            "operating-system randomness is unavailable: {error}"
        ))
    })?;
    write_new_key_file(output, &seed)?;
    let public = SigningKey::from_bytes(&seed).verifying_key();
    Ok(NewKey {
        key_id: KeyId::derive(&public),
        public_key: hex(&public.to_bytes()),
    })
}

/// The inputs of `index endorse`.
pub(crate) struct EndorseArgs {
    pub(crate) endorser_key_file: PathBuf,
    pub(crate) key_file: PathBuf,
    pub(crate) scope: String,
    pub(crate) not_before: u64,
    pub(crate) not_after: u64,
    pub(crate) output: PathBuf,
}

/// What `index endorse` reports: the record's key id and its endorser.
pub(crate) struct Endorsed {
    pub(crate) key_id: KeyId,
    pub(crate) endorsed_by: KeyId,
}

/// Refuses an endorsement window the index verifier or the ceremony forbids.
///
/// A key must outlive at least one index document
/// ([`MAX_INDEX_LIFETIME_SECONDS`]), must not be valid for longer than
/// [`MAX_INDEX_KEY_WINDOW_SECONDS`] and must not have expired already.
fn check_window(not_before: u64, not_after: u64, now: u64) -> Result<(), XtaskError> {
    let refuse = |reason: &str| Err(XtaskError::Usage(format!("the key window {reason}")));
    if not_after <= not_before {
        return refuse("is empty: --not-after must be greater than --not-before");
    }
    let window = not_after - not_before;
    if window < MAX_INDEX_LIFETIME_SECONDS {
        return refuse(&format!(
            "is shorter than one index lifetime ({MAX_INDEX_LIFETIME_SECONDS} seconds)"
        ));
    }
    if window > MAX_INDEX_KEY_WINDOW_SECONDS {
        return refuse(&format!(
            "is longer than {MAX_INDEX_KEY_WINDOW_SECONDS} seconds (366 days)"
        ));
    }
    if not_after <= now {
        return refuse("has already ended: --not-after is not in the future");
    }
    Ok(())
}

/// Checks `record` with the rules the index verifier applies to a key record
/// endorsed by `endorser`.
fn verify_record(record: &IndexKeyRecord, endorser: &VerifyingKey) -> Result<(), XtaskError> {
    let refuse = |reason: &str| {
        Err(XtaskError::Usage(format!(
            "the endorsement record is invalid: {reason}"
        )))
    };
    let public = hex_array::<32>(&record.public_key)
        .ok_or_else(|| XtaskError::Usage("the endorsed public key is malformed".into()))?;
    let key = VerifyingKey::from_bytes(&public)
        .map_err(|_cause| XtaskError::Usage("the endorsed public key is malformed".into()))?;
    if key.is_weak() {
        return refuse("the endorsed key is weak");
    }
    if KeyId::derive(&key) != record.key_id {
        return refuse("the key id does not match the public key");
    }
    if record.key_id == KeyId::derive(endorser) {
        return refuse("a root cannot endorse itself as an index key");
    }
    if record.not_after <= record.not_before {
        return refuse("the window is empty");
    }
    if record.not_after - record.not_before > MAX_INDEX_KEY_WINDOW_SECONDS {
        return refuse("the window is longer than 366 days");
    }
    if record.scope != OFFICIAL_SCOPE {
        return refuse("the scope is not `official`");
    }
    if record.endorsed_by != KeyId::derive(endorser) {
        return refuse("the endorser id does not match the endorser key");
    }
    let signature = hex_array::<64>(&record.signature)
        .map(|raw| Signature::from_bytes(&raw))
        .ok_or_else(|| XtaskError::Usage("the endorsement signature is malformed".into()))?;
    endorser
        .verify_strict(&index_key_record_signing_message(record), &signature)
        .map_err(|_cause| XtaskError::Usage("the endorsement signature does not verify".into()))
}

/// Decodes exactly `N` bytes from lowercase hex.
fn hex_array<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 {
        return None;
    }
    let nibble = |digit: u8| match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    };
    let mut out = [0_u8; N];
    let (pairs, _remainder) = text.as_bytes().as_chunks::<2>();
    for (slot, [high, low]) in out.iter_mut().zip(pairs) {
        *slot = (nibble(*high)? << 4) | nibble(*low)?;
    }
    Some(out)
}

/// Endorses the key in `args.key_file` as an official index key.
///
/// Every refusal and the verification of the signed record happen before the
/// output file is created.
pub(crate) fn endorse(args: &EndorseArgs, now: u64) -> Result<Endorsed, XtaskError> {
    if args.scope != OFFICIAL_SCOPE {
        return Err(XtaskError::Usage(format!(
            "--scope must be `{OFFICIAL_SCOPE}`"
        )));
    }
    check_window(args.not_before, args.not_after, now)?;
    let endorser = read_signing_key(&args.endorser_key_file)?;
    let index_key = read_signing_key(&args.key_file)?;

    let endorser_public = endorser.verifying_key();
    let index_public = index_key.verifying_key();
    let mut record = IndexKeyRecord {
        key_id: KeyId::derive(&index_public),
        public_key: hex(&index_public.to_bytes()),
        scope: args.scope.clone(),
        not_before: args.not_before,
        not_after: args.not_after,
        endorsed_by: KeyId::derive(&endorser_public),
        signature: String::new(),
    };
    let signature = endorser.sign(&index_key_record_signing_message(&record));
    record.signature = hex(&signature.to_bytes());
    verify_record(&record, &endorser_public)?;

    write_new_document(&args.output, &document_bytes(&record)?)?;
    Ok(Endorsed {
        key_id: record.key_id,
        endorsed_by: record.endorsed_by,
    })
}

/// The inputs of `index revoke`.
pub(crate) struct RevokeArgs {
    pub(crate) endorser_key_file: PathBuf,
    pub(crate) sequence: u64,
    pub(crate) key_ids: Vec<String>,
    pub(crate) previous: Option<PathBuf>,
    pub(crate) output: PathBuf,
}

/// What `index revoke` reports.
pub(crate) struct Revoked {
    pub(crate) sequence: u64,
    pub(crate) revoked: usize,
}

/// Checks `document` as a revocation list signed by `endorser`.
fn verify_revocations(
    document: &SignedRevocations,
    endorser: &VerifyingKey,
) -> Result<(), XtaskError> {
    let refuse = |reason: &str| {
        Err(XtaskError::Usage(format!(
            "the revocation document is invalid: {reason}"
        )))
    };
    let body = &document.revocations;
    if body.schema_version != INDEX_SCHEMA_VERSION {
        return refuse("unsupported schema version");
    }
    if body.revoked_key_ids.len() > MAX_REVOKED_KEYS {
        return refuse("too many revoked key ids");
    }
    if document.signature.key_id != KeyId::derive(endorser) {
        return refuse("it is not signed by the endorser key");
    }
    let signature = hex_array::<64>(&document.signature.signature)
        .map(|raw| Signature::from_bytes(&raw))
        .ok_or_else(|| XtaskError::Usage("the revocation signature is malformed".into()))?;
    let message = revocation_signing_message(body)
        .map_err(|error| XtaskError::Usage(format!("the revocation list is invalid: {error}")))?;
    endorser
        .verify_strict(&message, &signature)
        .map_err(|_cause| XtaskError::Usage("the revocation signature does not verify".into()))
}

/// Reads and verifies the `--previous` document against the endorser.
fn read_previous(path: &Path, endorser: &VerifyingKey) -> Result<RevocationBody, XtaskError> {
    let bytes = read_input(path, MAX_REVOCATIONS_FILE_BYTES)?;
    let document: SignedRevocations = serde_json::from_slice(&bytes).map_err(|_cause| {
        XtaskError::Usage(format!("`{}` is not a revocation document", path.display()))
    })?;
    verify_revocations(&document, endorser)?;
    Ok(document.revocations)
}

/// Refuses a revocation list that does not extend `previous`: its sequence
/// must rise and it must keep every previously revoked id.
fn check_successor(
    previous: &RevocationBody,
    sequence: u64,
    revoked: &BTreeSet<KeyId>,
) -> Result<(), XtaskError> {
    if sequence <= previous.sequence {
        return Err(XtaskError::Usage(format!(
            "--sequence must be greater than the previous sequence {}",
            previous.sequence
        )));
    }
    if let Some(dropped) = previous
        .revoked_key_ids
        .iter()
        .find(|id| !revoked.contains(*id))
    {
        return Err(XtaskError::Usage(format!(
            "a revocation list never shrinks: key id {dropped} of the previous document is missing"
        )));
    }
    Ok(())
}

/// Writes the next revocation document, signed by the endorser.
///
/// The new list is exactly `args.key_ids`. With `--previous` the sequence must
/// rise and the list must keep every previously revoked id; a first document
/// has sequence 1. A list never shrinks, and the endorser never revokes
/// itself: that is an anchor change in a core release.
pub(crate) fn revoke(args: &RevokeArgs) -> Result<Revoked, XtaskError> {
    let mut revoked = BTreeSet::new();
    for text in &args.key_ids {
        revoked.insert(parse_key_id(text, "--key-id")?);
    }
    if revoked.len() > MAX_REVOKED_KEYS {
        return Err(XtaskError::Usage(format!(
            "more than {MAX_REVOKED_KEYS} revoked key ids"
        )));
    }
    let endorser = read_signing_key(&args.endorser_key_file)?;
    let endorser_public = endorser.verifying_key();
    let endorser_id = KeyId::derive(&endorser_public);
    if revoked.contains(&endorser_id) {
        return Err(XtaskError::Usage(
            "the endorser key cannot revoke itself; revoking it is an anchor change in a core release"
                .into(),
        ));
    }

    match &args.previous {
        None if args.sequence != FIRST_REVOCATION_SEQUENCE => {
            return Err(XtaskError::Usage(format!(
                "without --previous only --sequence {FIRST_REVOCATION_SEQUENCE} is accepted"
            )));
        }
        None => {}
        Some(path) => {
            let previous = read_previous(path, &endorser_public)?;
            check_successor(&previous, args.sequence, &revoked)?;
        }
    }

    let body = RevocationBody {
        schema_version: INDEX_SCHEMA_VERSION,
        sequence: args.sequence,
        revoked_key_ids: revoked.into_iter().collect(),
    };
    let message = revocation_signing_message(&body)
        .map_err(|error| XtaskError::Usage(format!("the revocation list is invalid: {error}")))?;
    let signature = endorser.sign(&message);
    let document = SignedRevocations {
        revocations: body,
        signature: CatalogSignature {
            key_id: endorser_id,
            signature: hex(&signature.to_bytes()),
        },
    };
    verify_revocations(&document, &endorser_public)?;

    write_new_document(&args.output, &document_bytes(&document)?)?;
    Ok(Revoked {
        sequence: document.revocations.sequence,
        revoked: document.revocations.revoked_key_ids.len(),
    })
}

/// The inputs of `index channel`.
pub(crate) struct ChannelArgs {
    pub(crate) anchor: PathBuf,
    pub(crate) endorser_key_id: String,
    pub(crate) output: PathBuf,
}

/// Writes the channel document naming `args.endorser_key_id`, which must be a
/// root of the anchor.
pub(crate) fn channel(args: &ChannelArgs) -> Result<(), XtaskError> {
    let endorser_id = parse_key_id(&args.endorser_key_id, "--endorser-key-id")?;
    let anchor_bytes = read_input(
        &args.anchor,
        u64::try_from(MAX_ANCHOR_BYTES).unwrap_or(u64::MAX),
    )?;
    let anchor = parse_anchor(&anchor_bytes).map_err(XtaskError::Anchor)?;
    if anchor.revoked_key_ids().contains(&endorser_id) {
        return Err(XtaskError::Usage(
            "the endorser key is revoked by the trust anchor; no index could verify under it"
                .into(),
        ));
    }
    let channel = IndexChannel::new(endorser_id);
    channel
        .endorser(&anchor)
        .map_err(|error| XtaskError::Usage(error.to_string()))?;
    let bytes = channel
        .to_bytes()
        .map_err(|error| XtaskError::Usage(error.to_string()))?;
    write_new_document(&args.output, &bytes)
}
