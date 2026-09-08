//! Maintains independently durable relay recovery evidence.

// Rust guideline compliant 2026-09-08

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Mutex;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use nix::fcntl::{Flock, FlockArg};
use nix::unistd::Uid;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Limits one witness record to compact non-secret recovery metadata.
const MAX_WITNESS_BYTES: u64 = 64 * 1024;
/// Only the relay owner may traverse independently protected witness storage.
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
/// Only the relay owner may read or replace an individual witness record.
const PRIVATE_FILE_MODE: u32 = 0o600;
/// Keeps one operator review bounded while retaining each actionable scope.
const MAX_REVIEW_INCIDENTS: usize = 256;
/// A separate durable marker prevents a torn clean checkpoint from masking activity.
const ACTIVE_RUN_LATCH: &str = "witness.active";

/// Stores signed witness history independently from `PostgreSQL`.
pub struct WitnessStore {
    directory: PathBuf,
    signing_key: SigningKey,
    verifying_key: VerifyingKey,
    key_id: String,
    #[cfg(test)]
    failpoint: Mutex<Option<&'static str>>,
    #[cfg(test)]
    scan_incident_peak: Mutex<usize>,
}

impl fmt::Debug for WitnessStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WitnessStore")
            .field("key_id", &self.key_id)
            .field("verifying_key", &self.verifying_key)
            .finish_non_exhaustive()
    }
}

/// Reports witness durability or verification failure.
#[derive(Debug, Error)]
pub enum RecoveryError {
    /// The witness directory or record was unsafe or unavailable.
    #[error("relay recovery witness storage is unavailable")]
    Io(#[source] std::io::Error),
    /// Witness encoding or signature verification failed.
    #[error("relay recovery witness is invalid")]
    InvalidWitness,
    /// The expected witness was no longer current.
    #[error("relay recovery witness changed concurrently")]
    StaleWitness,
}

/// Captures one signed append-only recovery checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WitnessRecord {
    /// Relay deployment coordinate.
    pub relay_id: String,
    /// Recovery invalidation generation.
    pub recovery_generation: i64,
    /// Append-only witness sequence.
    pub sequence: i64,
    /// Operator-reviewed manifest digest encoded as lowercase hex.
    pub manifest_digest: String,
    /// Active signing key coordinate.
    pub key_id: String,
    /// Previous checkpoint digest, absent only at bootstrap.
    pub previous_digest: Option<String>,
    /// Whether the prior process completed orderly shutdown.
    pub active_run: bool,
    /// Whether only an audited recovery transition may clear this active latch.
    pub recovery_pending_review: bool,
    /// Names the durable reason for this checkpoint without overwriting review data.
    pub event: WitnessEvent,
    /// Carries a bounded actionable denial coordinate when this is an incident.
    pub incident: Option<DenyIncident>,
    /// Digest of denial scopes requiring reconciliation for a restore review.
    pub incident_digest: String,
    /// Signature over canonical unsigned fields.
    pub signature: String,
}

/// Names the durable checkpoint transition.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum WitnessEvent {
    /// A normal runtime checkpoint.
    Run,
    /// A protected bootstrap, bound to its exact identity and audit coordinates.
    Bootstrap {
        request_digest: [u8; 32],
        principal_id: Uuid,
        identity_id: Uuid,
        audit_id: Uuid,
    },
    /// A protected migration, bound to the exact binary plan and prior authority.
    Migration {
        plan_digest: [u8; 32],
        authority_digest: [u8; 32],
    },
    /// Recovery generation advanced before a database restore.
    RecoveryAdvance,
    /// A revocation denial became independently durable.
    DenyIncident,
    /// A reviewed recovery became clean after its database audit committed.
    RecoveryReviewed,
}

/// Identifies the canonical target of a durable denial incident.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum DenyIncident {
    /// The whole relay is denied.
    Global { relay_id: String },
    /// A team authority coordinate is denied.
    Team { team_id: Uuid },
    /// A principal authority coordinate is denied.
    Principal { principal_id: Uuid },
    /// A credential authority coordinate is denied.
    Credential { credential_id: Uuid },
    /// A membership authority coordinate is denied.
    Membership { membership_id: Uuid },
    /// A team policy coordinate is denied.
    Policy { team_id: Uuid },
    /// A recovery operation quarantines the whole relay.
    Recovery { relay_id: String },
}

/// Contains bounded independently witnessed denial scopes for operator recovery review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncidentReview {
    incidents: Vec<DenyIncident>,
    digest: [u8; 32],
}

struct HistoryScan {
    latest: Option<WitnessRecord>,
    latest_active: Option<WitnessRecord>,
    incidents: BTreeSet<DenyIncident>,
    overflow: bool,
}

impl IncidentReview {
    /// Returns each distinct denial scope requiring explicit reconciliation.
    #[must_use]
    pub fn incidents(&self) -> &[DenyIncident] {
        &self.incidents
    }

    /// Returns the canonical digest bound into the restore witness.
    #[must_use]
    pub const fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    pub(crate) fn from_scopes(incidents: &[DenyIncident]) -> Result<Self, RecoveryError> {
        Ok(Self {
            incidents: incidents.to_vec(),
            digest: digest_incidents(incidents)?,
        })
    }
}

impl WitnessStore {
    /// Opens independently protected witness storage.
    ///
    /// # Errors
    /// Returns [`RecoveryError::Io`] when the directory is missing or unsafe.
    pub fn open(
        directory: impl AsRef<Path>,
        signing_key: SigningKey,
        key_id: String,
    ) -> Result<Self, RecoveryError> {
        let directory = directory.as_ref().to_path_buf();
        ensure_private_directory(&directory)?;
        let verifying_key = signing_key.verifying_key();
        Ok(Self {
            directory,
            signing_key,
            verifying_key,
            key_id,
            #[cfg(test)]
            failpoint: Mutex::new(None),
            #[cfg(test)]
            scan_incident_peak: Mutex::new(0),
        })
    }

    /// Reads and verifies the latest witness checkpoint.
    pub fn latest(&self) -> Result<Option<WitnessRecord>, RecoveryError> {
        let history = self.scan_history()?;
        if self.active_latch_exists()? {
            return history
                .latest_active
                .ok_or(RecoveryError::InvalidWitness)
                .map(Some);
        }
        Ok(history.latest)
    }

    /// Returns denial scopes recorded since the last reviewed recovery checkpoint.
    pub(crate) fn incident_review(
        &self,
        checkpoint: &WitnessRecord,
    ) -> Result<IncidentReview, RecoveryError> {
        let history = self.scan_history()?;
        if history.latest.as_ref() != Some(checkpoint) {
            return Err(RecoveryError::StaleWitness);
        }
        if history.overflow {
            let relay_id = checkpoint.relay_id.clone();
            let incidents = vec![DenyIncident::Global { relay_id }];
            return Ok(IncidentReview {
                digest: digest_incidents(&incidents)?,
                incidents,
            });
        }
        let incidents = history.incidents.into_iter().collect::<Vec<_>>();
        Ok(IncidentReview {
            digest: digest_incidents(&incidents)?,
            incidents,
        })
    }

    /// Reconstructs witnessed incidents through one verified historical checkpoint.
    pub(crate) fn incident_review_at(
        &self,
        checkpoint: &WitnessRecord,
    ) -> Result<IncidentReview, RecoveryError> {
        let history = self.scan_history()?;
        if history
            .latest
            .as_ref()
            .is_none_or(|latest| latest.sequence < checkpoint.sequence)
        {
            return Err(RecoveryError::StaleWitness);
        }
        let stored = self.stored_expected(checkpoint)?;
        let mut previous = None;
        let mut incidents = BTreeSet::new();
        let mut overflow = false;
        for sequence in 1..=stored.sequence {
            let path = self.directory.join(format!("witness.{sequence:020}.json"));
            let record = read_record(&path)?;
            if record.sequence != sequence {
                return Err(RecoveryError::InvalidWitness);
            }
            self.verify(&record)?;
            if record.previous_digest != previous.as_ref().map(digest_record) {
                return Err(RecoveryError::InvalidWitness);
            }
            validate_transition(previous.as_ref(), &record)?;
            if matches!(record.event, WitnessEvent::RecoveryReviewed) {
                incidents.clear();
                overflow = false;
            } else if let Some(incident) = record.incident.clone() {
                if !overflow {
                    incidents.insert(incident);
                    if incidents.len() > MAX_REVIEW_INCIDENTS {
                        overflow = true;
                        incidents.clear();
                    }
                }
            }
            previous = Some(record);
        }
        if previous.as_ref() != Some(checkpoint) {
            return Err(RecoveryError::StaleWitness);
        }
        if overflow {
            let incidents = vec![DenyIncident::Global {
                relay_id: checkpoint.relay_id.clone(),
            }];
            return Ok(IncidentReview {
                digest: digest_incidents(&incidents)?,
                incidents,
            });
        }
        let incidents = incidents.into_iter().collect::<Vec<_>>();
        Ok(IncidentReview {
            digest: digest_incidents(&incidents)?,
            incidents,
        })
    }

    /// Appends a verified next checkpoint and atomically updates the current pointer.
    ///
    /// # Errors
    /// Returns [`RecoveryError::StaleWitness`] when the expected sequence no longer matches.
    fn append(
        &self,
        expected: Option<&WitnessRecord>,
        next: WitnessRecord,
    ) -> Result<WitnessRecord, RecoveryError> {
        let lock = self.lock_witness()?;
        let _lock = Flock::lock(lock, FlockArg::LockExclusiveNonblock).map_err(
            |(_file, error)| match error {
                nix::errno::Errno::EWOULDBLOCK => RecoveryError::StaleWitness,
                _ => RecoveryError::Io(std::io::Error::from(error)),
            },
        )?;
        self.append_locked(expected, next)
    }

    fn append_clean(
        &self,
        expected: &WitnessRecord,
        next: WitnessRecord,
    ) -> Result<WitnessRecord, RecoveryError> {
        let lock = self.lock_witness()?;
        let _lock = Flock::lock(lock, FlockArg::LockExclusiveNonblock).map_err(
            |(_file, error)| match error {
                nix::errno::Errno::EWOULDBLOCK => RecoveryError::StaleWitness,
                _ => RecoveryError::Io(std::io::Error::from(error)),
            },
        )?;
        let latch_exists = self.active_latch_exists()?;
        if !latch_exists {
            let current = self.stored_expected(expected)?;
            let clean = self.canonical_child(Some(&current), next)?;
            if !self.exact_child_exists(&clean)? {
                return Err(RecoveryError::StaleWitness);
            }
            self.repair_current(&clean)?;
            return Ok(clean);
        }
        let clean = self.append_locked(Some(expected), next)?;
        self.remove_active_latch()?;
        Ok(clean)
    }

    fn lock_witness(&self) -> Result<File, RecoveryError> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(PRIVATE_FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.directory.join("witness.lock"))
            .map_err(RecoveryError::Io)?;
        ensure_private_file(&lock)?;
        Ok(lock)
    }

    fn append_locked(
        &self,
        expected: Option<&WitnessRecord>,
        next: WitnessRecord,
    ) -> Result<WitnessRecord, RecoveryError> {
        let current = match expected {
            Some(expected) => Some(self.stored_expected(expected)?),
            None => self.latest_unlatched()?,
        };
        if current.as_ref().map(|record| record.sequence) != expected.map(|record| record.sequence)
        {
            return Err(RecoveryError::StaleWitness);
        }
        let next = self.canonical_child(current.as_ref(), next)?;
        if next.active_run {
            self.create_active_latch()?;
        }
        let history = self
            .directory
            .join(format!("witness.{:020}.json", next.sequence));
        let successor = self
            .directory
            .join(format!("witness.{:020}.json", next.sequence + 1));
        if successor.exists() {
            return Err(RecoveryError::StaleWitness);
        }
        match read_record_if_present(&history)? {
            Some(existing) if existing == next => {}
            Some(_) => return Err(RecoveryError::StaleWitness),
            None => self.write_new_synced(&history, &next)?,
        }
        self.write_synced(&self.directory.join("witness.current"), &next)?;
        self.fail("publication_dirsync")?;
        sync_directory(&self.directory)?;
        Ok(next)
    }

    fn stored_expected(&self, expected: &WitnessRecord) -> Result<WitnessRecord, RecoveryError> {
        let path = self
            .directory
            .join(format!("witness.{:020}.json", expected.sequence));
        let stored = read_record(&path)?;
        self.verify(&stored)?;
        if stored != *expected {
            return Err(RecoveryError::StaleWitness);
        }
        Ok(stored)
    }

    fn canonical_child(
        &self,
        current: Option<&WitnessRecord>,
        mut next: WitnessRecord,
    ) -> Result<WitnessRecord, RecoveryError> {
        validate_transition(current, &next)?;
        next.sequence = current.map_or(1, |record| record.sequence + 1);
        next.previous_digest = current.map(digest_record);
        next.key_id = self.key_id.clone();
        next.signature = signature_text(&self.signing_key, &unsigned_bytes(&next)?);
        self.verify(&next)?;
        Ok(next)
    }

    fn exact_child_exists(&self, child: &WitnessRecord) -> Result<bool, RecoveryError> {
        let history = self
            .directory
            .join(format!("witness.{:020}.json", child.sequence));
        let successor = self
            .directory
            .join(format!("witness.{:020}.json", child.sequence + 1));
        if successor.exists() {
            return Err(RecoveryError::StaleWitness);
        }
        match read_record_if_present(&history)? {
            Some(existing) if existing == *child => Ok(true),
            Some(_) => Err(RecoveryError::StaleWitness),
            None => Ok(false),
        }
    }

    fn repair_current(&self, child: &WitnessRecord) -> Result<(), RecoveryError> {
        let current = self.directory.join("witness.current");
        if read_record_if_present(&current)?.as_ref() != Some(child) {
            self.write_synced(&current, child)?;
        }
        self.fail("publication_dirsync")?;
        sync_directory(&self.directory)
    }

    /// Persists a bounded deny incident before a revocation acknowledgement.
    ///
    /// # Errors
    /// Returns an error when the incident cannot become durable; callers must
    /// leave the active-run latch dirty and stop rather than acknowledge.
    pub fn record_deny_incident(
        &self,
        current: &WitnessRecord,
        incident: DenyIncident,
    ) -> Result<WitnessRecord, RecoveryError> {
        if current.recovery_pending_review || !incident_is_valid(&incident) {
            return Err(RecoveryError::InvalidWitness);
        }
        let mut next = current.clone();
        next.active_run = true;
        next.event = WitnessEvent::DenyIncident;
        next.incident = Some(incident);
        self.append(Some(current), next)
    }

    /// Records an unclean active run before ingress is enabled.
    pub fn begin_run(
        &self,
        current: Option<&WitnessRecord>,
        relay_id: &str,
        generation: i64,
    ) -> Result<WitnessRecord, RecoveryError> {
        self.append(
            current,
            WitnessRecord {
                relay_id: relay_id.to_owned(),
                recovery_generation: generation,
                sequence: 0,
                manifest_digest: "0".repeat(64),
                key_id: String::new(),
                previous_digest: None,
                active_run: true,
                recovery_pending_review: false,
                event: WitnessEvent::Run,
                incident: None,
                incident_digest: "0".repeat(64),
                signature: String::new(),
            },
        )
    }

    /// Records an operation-specific latch before a protected local database write.
    pub(crate) fn begin_local(
        &self,
        current: Option<&WitnessRecord>,
        relay_id: &str,
        generation: i64,
        event: WitnessEvent,
    ) -> Result<WitnessRecord, RecoveryError> {
        if !matches!(
            event,
            WitnessEvent::Bootstrap { .. } | WitnessEvent::Migration { .. }
        ) {
            return Err(RecoveryError::InvalidWitness);
        }
        let mut next = match current {
            Some(current) if !current.active_run => current.clone(),
            Some(_) => return Err(RecoveryError::InvalidWitness),
            None => WitnessRecord {
                relay_id: relay_id.to_owned(),
                recovery_generation: generation,
                sequence: 0,
                manifest_digest: "0".repeat(64),
                key_id: String::new(),
                previous_digest: None,
                active_run: true,
                recovery_pending_review: false,
                event,
                incident: None,
                incident_digest: "0".repeat(64),
                signature: String::new(),
            },
        };
        next.active_run = true;
        next.event = event;
        next.incident = None;
        self.append(current, next)
    }

    /// Publishes the exact clean local-operation child after the database commit.
    pub(crate) fn complete_local(
        &self,
        current: &WitnessRecord,
    ) -> Result<WitnessRecord, RecoveryError> {
        if current.recovery_pending_review
            || !matches!(
                current.event,
                WitnessEvent::Bootstrap { .. } | WitnessEvent::Migration { .. }
            )
        {
            return Err(RecoveryError::InvalidWitness);
        }
        let mut next = current.clone();
        next.active_run = false;
        self.append_clean(current, next)
    }

    /// Records orderly shutdown only after forwarding has stopped.
    pub fn end_run(&self, current: &WitnessRecord) -> Result<WitnessRecord, RecoveryError> {
        if current.recovery_pending_review {
            return Err(RecoveryError::InvalidWitness);
        }
        let mut next = current.clone();
        next.active_run = false;
        next.event = WitnessEvent::Run;
        next.incident = None;
        self.append_clean(current, next)
    }

    /// Advances recovery generation before a database restore.
    pub fn advance_restore(
        &self,
        current: &WitnessRecord,
        manifest_digest: String,
        incident_digest: String,
    ) -> Result<WitnessRecord, RecoveryError> {
        if !is_sha256_hex(&incident_digest) {
            return Err(RecoveryError::InvalidWitness);
        }
        let mut next = current.clone();
        next.recovery_generation = next
            .recovery_generation
            .checked_add(1)
            .ok_or(RecoveryError::InvalidWitness)?;
        next.manifest_digest = manifest_digest;
        next.active_run = true;
        next.recovery_pending_review = true;
        next.event = WitnessEvent::RecoveryAdvance;
        next.incident = None;
        next.incident_digest = incident_digest;
        self.append(Some(current), next)
    }

    /// Clears a reviewed recovery latch after its database audit commits.
    pub(crate) fn complete_recovery_review(
        &self,
        current: &WitnessRecord,
        manifest_digest: &str,
    ) -> Result<WitnessRecord, RecoveryError> {
        let next = self.recovery_review_child(current, manifest_digest)?;
        self.append_clean(current, next)
    }

    /// Reports whether the exact reviewed-clean child was already published.
    pub(crate) fn recovery_review_is_published(
        &self,
        current: &WitnessRecord,
        manifest_digest: &str,
    ) -> Result<bool, RecoveryError> {
        self.stored_expected(current)?;
        let child = self.recovery_review_child(current, manifest_digest)?;
        self.exact_child_exists(&child)
    }

    fn recovery_review_child(
        &self,
        current: &WitnessRecord,
        manifest_digest: &str,
    ) -> Result<WitnessRecord, RecoveryError> {
        if !current.active_run
            || !current.recovery_pending_review
            || !is_sha256_hex(manifest_digest)
        {
            return Err(RecoveryError::InvalidWitness);
        }
        let mut next = current.clone();
        next.manifest_digest = manifest_digest.to_owned();
        next.active_run = false;
        next.recovery_pending_review = false;
        next.event = WitnessEvent::RecoveryReviewed;
        next.incident = None;
        self.canonical_child(Some(current), next)
    }

    fn verify(&self, record: &WitnessRecord) -> Result<(), RecoveryError> {
        if record.sequence <= 0
            || record.recovery_generation <= 0
            || record.key_id != self.key_id
            || !is_sha256_hex(&record.manifest_digest)
            || !is_sha256_hex(&record.incident_digest)
            || matches!(record.event, WitnessEvent::DenyIncident) != record.incident.is_some()
            || record
                .incident
                .as_ref()
                .is_some_and(|incident| !incident_is_valid(incident))
        {
            return Err(RecoveryError::InvalidWitness);
        }
        let signature =
            hex::decode(&record.signature).map_err(|_error| RecoveryError::InvalidWitness)?;
        let signature =
            Signature::from_slice(&signature).map_err(|_error| RecoveryError::InvalidWitness)?;
        self.verifying_key
            .verify(&unsigned_bytes(record)?, &signature)
            .map_err(|_error| RecoveryError::InvalidWitness)
    }

    fn scan_history(&self) -> Result<HistoryScan, RecoveryError> {
        #[cfg(test)]
        {
            *self
                .scan_incident_peak
                .lock()
                .map_err(|_error| RecoveryError::InvalidWitness)? = 0
        };
        let mut count = 0_i64;
        let mut max_sequence = 0_i64;
        for entry in fs::read_dir(&self.directory).map_err(RecoveryError::Io)? {
            let entry = entry.map_err(RecoveryError::Io)?;
            let file_name = entry.file_name();
            let Some(sequence) = parse_history_sequence(&file_name) else {
                continue;
            };
            count = count.checked_add(1).ok_or(RecoveryError::InvalidWitness)?;
            max_sequence = max_sequence.max(sequence);
        }
        if count != max_sequence {
            return Err(RecoveryError::InvalidWitness);
        }
        let pointer = read_record_if_present(&self.directory.join("witness.current"))?;
        if let Some(pointer) = &pointer {
            self.verify(pointer)?;
        }
        let mut previous: Option<WitnessRecord> = None;
        let mut latest_active = None;
        let mut pointer_matches = pointer.is_none();
        let mut incidents = BTreeSet::new();
        let mut overflow = false;
        for sequence in 1..=max_sequence {
            let path = self.directory.join(format!("witness.{sequence:020}.json"));
            let record = read_record(&path)?;
            if record.sequence != sequence {
                return Err(RecoveryError::InvalidWitness);
            }
            self.verify(&record)?;
            if record.previous_digest != previous.as_ref().map(digest_record) {
                return Err(RecoveryError::InvalidWitness);
            }
            validate_transition(previous.as_ref(), &record)?;
            if pointer.as_ref().is_some_and(|pointer| pointer == &record) {
                pointer_matches = true;
            }
            if record.active_run {
                latest_active = Some(record.clone());
            }
            if matches!(record.event, WitnessEvent::RecoveryReviewed) {
                incidents.clear();
                overflow = false;
            } else if let Some(incident) = record.incident.clone() {
                if !overflow {
                    incidents.insert(incident);
                    #[cfg(test)]
                    {
                        let mut peak = self
                            .scan_incident_peak
                            .lock()
                            .map_err(|_error| RecoveryError::InvalidWitness)?;
                        *peak = (*peak).max(incidents.len())
                    };
                    if incidents.len() > MAX_REVIEW_INCIDENTS {
                        overflow = true;
                        incidents.clear();
                    }
                }
            }
            previous = Some(record);
        }
        if !pointer_matches {
            return Err(RecoveryError::InvalidWitness);
        }
        Ok(HistoryScan {
            latest: previous,
            latest_active,
            incidents,
            overflow,
        })
    }

    fn latest_unlatched(&self) -> Result<Option<WitnessRecord>, RecoveryError> {
        Ok(self.scan_history()?.latest)
    }

    #[cfg(test)]
    fn scan_incident_peak(&self) -> usize {
        *self.scan_incident_peak.lock().expect("scan peak lock")
    }

    fn active_latch_exists(&self) -> Result<bool, RecoveryError> {
        match fs::symlink_metadata(self.directory.join(ACTIVE_RUN_LATCH)) {
            Ok(metadata) if metadata.file_type().is_file() => Ok(true),
            Ok(_) => Err(RecoveryError::InvalidWitness),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(RecoveryError::Io(error)),
        }
    }

    fn create_active_latch(&self) -> Result<(), RecoveryError> {
        let path = self.directory.join(ACTIVE_RUN_LATCH);
        if self.active_latch_exists()? {
            return Ok(());
        }
        let mut latch = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PRIVATE_FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(RecoveryError::Io)?;
        self.fail("latch_write")?;
        latch.write_all(b"active\n").map_err(RecoveryError::Io)?;
        self.fail("latch_fsync")?;
        latch.sync_all().map_err(RecoveryError::Io)?;
        self.fail("latch_dirsync")?;
        sync_directory(&self.directory)
    }

    fn remove_active_latch(&self) -> Result<(), RecoveryError> {
        let path = self.directory.join(ACTIVE_RUN_LATCH);
        self.fail("preunlink")?;
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(RecoveryError::Io(error)),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_failpoint(&self, stage: Option<&'static str>) {
        *self.failpoint.lock().expect("failpoint lock") = stage;
    }

    #[cfg_attr(
        not(test),
        expect(
            clippy::unused_self,
            clippy::unnecessary_wraps,
            reason = "Production and fault-injected builds retain the same durability boundary"
        )
    )]
    fn fail(&self, stage: &'static str) -> Result<(), RecoveryError> {
        let _ = stage;
        #[cfg(test)]
        if self
            .failpoint
            .lock()
            .map_err(|_error| RecoveryError::InvalidWitness)?
            .as_deref()
            == Some(stage)
        {
            return Err(RecoveryError::Io(std::io::Error::other("test I/O failure")));
        }
        Ok(())
    }

    fn write_new_synced(&self, path: &Path, record: &WitnessRecord) -> Result<(), RecoveryError> {
        if path.exists() {
            return Err(RecoveryError::StaleWitness);
        }
        self.write_synced(path, record)
    }

    fn write_synced(&self, path: &Path, record: &WitnessRecord) -> Result<(), RecoveryError> {
        let is_current = path
            .file_name()
            .is_some_and(|name| name == "witness.current");
        let temporary = path.with_extension(format!("{}.tmp", Uuid::now_v7()));
        let bytes = serde_json::to_vec(record).map_err(|_error| RecoveryError::InvalidWitness)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PRIVATE_FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)
            .map_err(RecoveryError::Io)?;
        self.fail(if is_current {
            "current_write"
        } else {
            "history_write"
        })?;
        file.write_all(&bytes).map_err(RecoveryError::Io)?;
        self.fail(if is_current {
            "current_fsync"
        } else {
            "history_fsync"
        })?;
        file.sync_all().map_err(RecoveryError::Io)?;
        self.fail(if is_current {
            "current_rename"
        } else {
            "history_rename"
        })?;
        fs::rename(&temporary, path).map_err(RecoveryError::Io)?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(RecoveryError::Io)?;
        ensure_private_file(&file)
    }
}

fn unsigned_bytes(record: &WitnessRecord) -> Result<Vec<u8>, RecoveryError> {
    serde_json::to_vec(&(
        &record.relay_id,
        record.recovery_generation,
        record.sequence,
        &record.manifest_digest,
        &record.key_id,
        &record.previous_digest,
        record.active_run,
        record.recovery_pending_review,
        &record.event,
        &record.incident,
        &record.incident_digest,
    ))
    .map_err(|_error| RecoveryError::InvalidWitness)
}

fn digest_record(record: &WitnessRecord) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(
        serde_json::to_vec(record).expect("serializable witness record"),
    ))
}

fn digest_incidents(incidents: &[DenyIncident]) -> Result<[u8; 32], RecoveryError> {
    use sha2::{Digest, Sha256};
    let encoded = serde_json::to_vec(incidents).map_err(|_error| RecoveryError::InvalidWitness)?;
    let mut hash = Sha256::new();
    hash.update(b"pohunek.relay.recovery-incidents.v1\0");
    hash.update(encoded);
    Ok(hash.finalize().into())
}

fn validate_transition(
    current: Option<&WitnessRecord>,
    next: &WitnessRecord,
) -> Result<(), RecoveryError> {
    let Some(current) = current else {
        if next.recovery_generation <= 0 || next.relay_id.is_empty() {
            return Err(RecoveryError::InvalidWitness);
        }
        return Ok(());
    };
    if next.relay_id != current.relay_id || next.recovery_generation < current.recovery_generation {
        return Err(RecoveryError::InvalidWitness);
    }
    if next.recovery_generation > current.recovery_generation
        && (!next.active_run || !next.recovery_pending_review)
    {
        return Err(RecoveryError::InvalidWitness);
    }
    if next.recovery_pending_review && !next.active_run {
        return Err(RecoveryError::InvalidWitness);
    }
    if matches!(next.event, WitnessEvent::DenyIncident) != next.incident.is_some()
        || next
            .incident
            .as_ref()
            .is_some_and(|incident| !incident_is_valid(incident))
    {
        return Err(RecoveryError::InvalidWitness);
    }
    if current.recovery_pending_review
        && (next.recovery_generation != current.recovery_generation
            || next.active_run
            || next.recovery_pending_review)
    {
        return Err(RecoveryError::InvalidWitness);
    }
    Ok(())
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn incident_is_valid(incident: &DenyIncident) -> bool {
    match incident {
        DenyIncident::Global { relay_id } | DenyIncident::Recovery { relay_id } => {
            !relay_id.is_empty() && relay_id.len() <= 256
        }
        DenyIncident::Team { .. }
        | DenyIncident::Principal { .. }
        | DenyIncident::Credential { .. }
        | DenyIncident::Membership { .. }
        | DenyIncident::Policy { .. } => true,
    }
}

fn signature_text(key: &SigningKey, bytes: &[u8]) -> String {
    hex::encode(key.sign(bytes).to_bytes())
}

fn ensure_private_directory(path: &Path) -> Result<(), RecoveryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => return Err(RecoveryError::InvalidWitness),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .mode(PRIVATE_DIRECTORY_MODE)
                .create(path)
                .map_err(RecoveryError::Io)?;
        }
        Err(error) => return Err(RecoveryError::Io(error)),
    }
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .map_err(RecoveryError::Io)?;
    let metadata = directory.metadata().map_err(RecoveryError::Io)?;
    if !metadata.is_dir()
        || metadata.uid() != Uid::effective().as_raw()
        || metadata.permissions().mode() & 0o777 != PRIVATE_DIRECTORY_MODE
    {
        return Err(RecoveryError::InvalidWitness);
    }
    Ok(())
}

fn ensure_private_file(file: &File) -> Result<(), RecoveryError> {
    let metadata = file.metadata().map_err(RecoveryError::Io)?;
    if !metadata.is_file()
        || metadata.uid() != Uid::effective().as_raw()
        || metadata.permissions().mode() & 0o777 != PRIVATE_FILE_MODE
    {
        return Err(RecoveryError::InvalidWitness);
    }
    Ok(())
}

fn read_record_if_present(path: &Path) -> Result<Option<WitnessRecord>, RecoveryError> {
    match read_record(path) {
        Ok(record) => Ok(Some(record)),
        Err(RecoveryError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_record(path: &Path) -> Result<WitnessRecord, RecoveryError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(RecoveryError::Io)?;
    ensure_private_file(&file)?;
    let metadata = file.metadata().map_err(RecoveryError::Io)?;
    if metadata.len() > MAX_WITNESS_BYTES {
        return Err(RecoveryError::InvalidWitness);
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len()).map_err(|_error| RecoveryError::InvalidWitness)?,
    );
    file.take(MAX_WITNESS_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(RecoveryError::Io)?;
    serde_json::from_slice(&bytes).map_err(|_error| RecoveryError::InvalidWitness)
}

fn sync_directory(directory: &Path) -> Result<(), RecoveryError> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(directory)
        .and_then(|file| file.sync_all())
        .map_err(RecoveryError::Io)
}

fn parse_history_sequence(file_name: &std::ffi::OsStr) -> Option<i64> {
    let value = file_name.to_str()?;
    let digits = value.strip_prefix("witness.")?.strip_suffix(".json")?;
    if digits.len() != 20 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use ed25519_dalek::SigningKey;
    use tempfile::tempdir;

    use super::{DenyIncident, RecoveryError, WitnessStore, MAX_REVIEW_INCIDENTS};

    fn fixture() -> (tempfile::TempDir, WitnessStore) {
        let directory = tempdir().expect("temporary witness directory");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private witness directory");
        let store = WitnessStore::open(
            directory.path(),
            SigningKey::from_bytes(&[8; 32]),
            "test-key".to_owned(),
        )
        .expect("open witness");
        (directory, store)
    }

    #[test]
    fn clean_publication_failpoints_retry_the_exact_child() {
        const STAGES: &[&str] = &[
            "latch_write",
            "latch_fsync",
            "latch_dirsync",
            "history_write",
            "history_fsync",
            "history_rename",
            "current_write",
            "current_fsync",
            "current_rename",
            "publication_dirsync",
            "preunlink",
        ];
        for stage in STAGES {
            let directory = tempdir().expect("temporary witness directory");
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .expect("private witness directory");
            let store = WitnessStore::open(
                directory.path(),
                SigningKey::from_bytes(&[3; 32]),
                "test-key".to_owned(),
            )
            .expect("open witness");
            if stage.starts_with("latch_") {
                store.set_failpoint(Some(stage));
                assert!(matches!(
                    store.begin_run(None, "relay-test", 1),
                    Err(RecoveryError::Io(_))
                ));
                store.set_failpoint(None);
                let active = store
                    .begin_run(None, "relay-test", 1)
                    .expect("retry active run");
                let clean = store.end_run(&active).expect("clean retry active child");
                assert_eq!(clean.sequence, 2, "stage {stage}");
                continue;
            }
            let active = store
                .begin_run(None, "relay-test", 1)
                .expect("begin active run");
            store.set_failpoint(Some(stage));
            assert!(matches!(store.end_run(&active), Err(RecoveryError::Io(_))));
            assert!(
                store.latest().is_err()
                    || store.latest().expect("latest").expect("record").active_run
            );
            store.set_failpoint(None);
            let clean = store.end_run(&active).expect("retry exact clean child");
            assert_eq!(clean.sequence, 2, "stage {stage}");
            assert!(!directory
                .path()
                .join("witness.00000000000000000003.json")
                .exists());
        }
    }

    #[test]
    fn clean_retry_rejects_a_different_canonical_child() {
        let directory = tempdir().expect("temporary witness directory");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private witness directory");
        let store = WitnessStore::open(
            directory.path(),
            SigningKey::from_bytes(&[4; 32]),
            "test-key".to_owned(),
        )
        .expect("open witness");
        let active = store
            .begin_run(None, "relay-test", 1)
            .expect("begin active run");
        store.set_failpoint(Some("current_write"));
        store.end_run(&active).unwrap_err();
        store.set_failpoint(None);
        let mut different = active.clone();
        different.active_run = false;
        different.event = super::WitnessEvent::Run;
        different.incident = None;
        different.manifest_digest = "a".repeat(64);
        assert!(matches!(
            store.append_clean(&active, different),
            Err(RecoveryError::StaleWitness)
        ));
        let clean = store.end_run(&active).expect("retry original child");
        assert_eq!(clean.sequence, 2);
    }

    #[test]
    fn incident_review_counts_only_distinct_denials_since_review() {
        const MANY: usize = 257;
        let (_directory, store) = fixture();
        let mut current = store.begin_run(None, "relay-test", 1).expect("begin run");
        for _ in 0..MANY {
            current = store
                .begin_run(Some(&current), "relay-test", 1)
                .expect("append run record");
        }
        assert!(store
            .incident_review(&current)
            .expect("review runs")
            .incidents()
            .is_empty());

        let (_directory, store) = fixture();
        let mut current = store.begin_run(None, "relay-test", 1).expect("begin run");
        let incident = DenyIncident::Principal {
            principal_id: uuid::Uuid::nil(),
        };
        for _ in 0..MANY {
            current = store
                .record_deny_incident(&current, incident.clone())
                .expect("append duplicate denial");
        }
        assert_eq!(
            store
                .incident_review(&current)
                .expect("review duplicates")
                .incidents(),
            &[incident]
        );

        let (_directory, store) = fixture();
        let mut current = store.begin_run(None, "relay-test", 1).expect("begin run");
        for _ in 0..MANY {
            current = store
                .record_deny_incident(
                    &current,
                    DenyIncident::Principal {
                        principal_id: uuid::Uuid::now_v7(),
                    },
                )
                .expect("append distinct denial");
        }
        assert_eq!(
            store
                .incident_review(&current)
                .expect("review overflow")
                .incidents(),
            &[DenyIncident::Global {
                relay_id: "relay-test".to_owned()
            }]
        );
        assert!(
            store.scan_incident_peak() <= MAX_REVIEW_INCIDENTS + 1,
            "the history scan must retain at most the overflow threshold"
        );
    }

    #[test]
    fn recovery_review_resets_an_overflow_before_the_next_incident() {
        const OVERFLOW_INCIDENTS: usize = MAX_REVIEW_INCIDENTS + 1;
        let (_directory, store) = fixture();
        let mut current = store.begin_run(None, "relay-test", 1).expect("begin run");
        for _ in 0..OVERFLOW_INCIDENTS {
            current = store
                .record_deny_incident(
                    &current,
                    DenyIncident::Principal {
                        principal_id: uuid::Uuid::now_v7(),
                    },
                )
                .expect("append overflow incident");
        }
        let overflow = store
            .incident_review(&current)
            .expect("read overflow review");
        let advanced = store
            .advance_restore(&current, "a".repeat(64), hex::encode(overflow.digest()))
            .expect("advance recovery");
        let reviewed = store
            .complete_recovery_review(&advanced, &"b".repeat(64))
            .expect("complete recovery review");
        let resumed = store
            .begin_run(Some(&reviewed), "relay-test", 2)
            .expect("resume after review");
        let new_incident = DenyIncident::Principal {
            principal_id: uuid::Uuid::now_v7(),
        };
        let current = store
            .record_deny_incident(&resumed, new_incident.clone())
            .expect("append post-review incident");
        assert_eq!(
            store
                .incident_review(&current)
                .expect("review post-review incident")
                .incidents(),
            &[new_incident]
        );
    }

    #[test]
    fn history_gap_and_pointer_mismatch_fail_closed() {
        let (directory, store) = fixture();
        let first = store.begin_run(None, "relay-test", 1).expect("begin run");
        let _clean = store.end_run(&first).expect("clean run");
        std::fs::remove_file(directory.path().join("witness.00000000000000000001.json"))
            .expect("remove first history record");
        assert!(matches!(store.latest(), Err(RecoveryError::InvalidWitness)));

        let (directory, store) = fixture();
        let _first = store.begin_run(None, "relay-test", 1).expect("begin run");
        let foreign_directory = tempdir().expect("foreign witness directory");
        std::fs::set_permissions(
            foreign_directory.path(),
            std::fs::Permissions::from_mode(0o700),
        )
        .expect("private foreign witness directory");
        let foreign = WitnessStore::open(
            foreign_directory.path(),
            SigningKey::from_bytes(&[8; 32]),
            "test-key".to_owned(),
        )
        .expect("open foreign witness");
        let _foreign = foreign
            .begin_run(None, "other-relay", 1)
            .expect("begin foreign run");
        std::fs::copy(
            foreign_directory.path().join("witness.current"),
            directory.path().join("witness.current"),
        )
        .expect("replace pointer with a valid foreign checkpoint");
        assert!(matches!(store.latest(), Err(RecoveryError::InvalidWitness)));
    }
}
