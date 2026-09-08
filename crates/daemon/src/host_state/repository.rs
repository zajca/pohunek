//! Locked durable repository for stable host identity and governance state.
//!
//! Opening the repository retains the descriptor-relative process lock for its
//! lifetime. Bootstrap therefore has one serializable decision point and never
//! selects an implicit winner from incomplete state.

// Rust guideline compliant 2026-09-04

use std::fmt::{Debug, Formatter};
use std::path::Path;
use std::time::Duration;

use protocol::{
    ApprovalKeyReference, HostApprovalSignature, HostGovernanceStatus, HostId,
    SignedTransferOutcome,
};
use zeroize::Zeroizing;

use super::{
    approval_key::{generate_host_id, ApprovalKey, ApprovalKeyRecord},
    records::{GovernanceState, IdentityRecord, RetiredEnrollment},
    HostStateDir, HostStateError, HostStateLock,
};

const IDENTITY_RECORD: &str = pohunek_paths::HOST_IDENTITY_NAME;
const APPROVAL_KEY_RECORD: &str = pohunek_paths::HOST_APPROVAL_KEY_NAME;
const GOVERNANCE_RECORD: &str = pohunek_paths::HOST_GOVERNANCE_NAME;
/// Domain-separated local proof used only to verify the retained signer path.
///
/// This fixed public payload can never be interpreted as a governance command
/// or transfer outcome, and its signature never leaves this repository.
const DIAGNOSTIC_SIGNING_PROBE: &[u8] = b"pohunek.host-governance.doctor.v1\0";
/// Short retry delay lets a concurrent bootstrap finish without an unsafe winner.
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(10);
/// Five seconds bounds startup blocking while a concurrently bootstrapping process exits.
const LOCK_RETRY_ATTEMPTS: usize = 500;

/// Failure while loading or durably updating stable host state.
#[derive(Debug, thiserror::Error)]
pub enum HostStateRepositoryError {
    /// Underlying safe descriptor-relative persistence failed.
    #[error("host-state persistence failed while handling {record}")]
    Persistence {
        /// Fixed record name whose operation failed.
        record: &'static str,
        /// Underlying no-follow persistence failure.
        #[source]
        source: HostStateError,
    },
    /// A durable record was malformed or violated a security invariant.
    #[error("host-state {record} record is invalid")]
    InvalidRecord {
        /// Fixed record name.
        record: &'static str,
    },
    /// A durable record used an unsupported future format version.
    #[error("host-state {record} record has an unsupported format version")]
    UnsupportedRecordVersion {
        /// Fixed record name.
        record: &'static str,
    },
    /// A schema-specific record ceiling was exceeded.
    #[error("host-state {record} record exceeds {max_bytes} bytes")]
    RecordTooLarge {
        /// Fixed record name.
        record: &'static str,
        /// Maximum accepted serialized bytes for this schema.
        max_bytes: usize,
    },
    /// A partial record set cannot be repaired without selecting a winner.
    #[error("host-state contains an unsafe incomplete record set")]
    IncompleteRecordSet,
    /// Operating-system randomness could not safely generate stable material.
    #[error("secure entropy was unavailable while generating {stage}")]
    Entropy {
        /// Non-secret generation stage.
        stage: &'static str,
    },
    /// Rename committed but directory synchronization left durability uncertain.
    #[error("host-state {record} commit has uncertain durability")]
    DurabilityUncertain {
        /// Fixed record name.
        record: &'static str,
    },
    /// A caller tried to write against stale durable governance state.
    #[error("host governance state changed before the requested replacement")]
    StaleGovernance,
}

/// Safe immutable inspection snapshot without secret or outcome data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostStateSnapshot {
    host_id: HostId,
    governance: HostGovernanceStatus,
}

impl HostStateSnapshot {
    /// Returns the stable host identity.
    #[must_use]
    pub const fn host_id(&self) -> &HostId {
        &self.host_id
    }

    /// Returns safe current governance inspection state.
    #[must_use]
    pub const fn governance(&self) -> &HostGovernanceStatus {
        &self.governance
    }
}

/// One locked stable-host repository, including its non-clonable approval signer.
pub struct HostStateRepository {
    directory: HostStateDir,
    _lock: HostStateLock,
    approval_key: ApprovalKey,
    governance: GovernanceState,
}

impl Debug for HostStateRepository {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostStateRepository")
            .field("snapshot", &self.snapshot())
            .field("approval_key", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl HostStateRepository {
    /// Opens one locked host-state repository and safely bootstraps missing records.
    ///
    /// The supplied path is the canonical XDG application state directory, not
    /// the `host` child. The returned repository retains the cross-process lock
    /// until dropped.
    ///
    /// # Errors
    ///
    /// Returns [`HostStateRepositoryError`] for unsafe filesystem state,
    /// malformed records, unavailable entropy, or unproven durable commits.
    pub fn open_or_create(state_dir: impl AsRef<Path>) -> Result<Self, HostStateRepositoryError> {
        let directory = HostStateDir::open_or_create(state_dir.as_ref()).map_err(|source| {
            HostStateRepositoryError::Persistence {
                record: "host directory",
                source,
            }
        })?;
        let lock = acquire_lock(&directory)?;
        Self::load_locked(directory, lock)
    }

    /// Returns the safe immutable snapshot for owner-facing services.
    #[must_use]
    pub fn snapshot(&self) -> HostStateSnapshot {
        HostStateSnapshot {
            host_id: self.governance.status().host_id().clone(),
            governance: self.governance.status().clone(),
        }
    }

    /// Revalidates the live durable state without exposing persistence details.
    ///
    /// The returned snapshot is safe for owner-facing diagnostics. This method
    /// reopens every managed name without following links, checks the current
    /// descriptors still name the retained repository, decodes and verifies
    /// every record, and proves that the internal signer verifies its own
    /// domain-separated local signature.
    ///
    /// # Errors
    ///
    /// Returns a typed repository error when durable state is missing, unsafe,
    /// malformed, inconsistent, or no longer matches the retained snapshot.
    pub(crate) fn diagnose(&self) -> Result<HostStateSnapshot, HostStateRepositoryError> {
        self.directory.validate_current_layout().map_err(|source| {
            HostStateRepositoryError::Persistence {
                record: "host directory",
                source,
            }
        })?;
        let identity = read(&self.directory, IDENTITY_RECORD)?
            .ok_or(HostStateRepositoryError::IncompleteRecordSet)
            .and_then(|bytes| IdentityRecord::decode(&bytes))?;
        let key = read_secret(&self.directory, APPROVAL_KEY_RECORD)?
            .ok_or(HostStateRepositoryError::IncompleteRecordSet)
            .and_then(|bytes| ApprovalKeyRecord::decode(&bytes))?;
        let governance = read(&self.directory, GOVERNANCE_RECORD)?
            .ok_or(HostStateRepositoryError::IncompleteRecordSet)
            .and_then(|bytes| GovernanceState::decode(&bytes))?;
        validate_governance(
            &governance,
            &identity.host_id,
            key.key.reference(),
            &key.key,
        )?;
        let snapshot = HostStateSnapshot {
            host_id: identity.host_id,
            governance: governance.status().clone(),
        };
        if snapshot != self.snapshot()
            || key.host_id != *snapshot.host_id()
            || key.key.reference() != self.approval_key.reference()
        {
            return Err(HostStateRepositoryError::InvalidRecord {
                record: GOVERNANCE_RECORD,
            });
        }
        let signature = self.approval_key.sign(DIAGNOSTIC_SIGNING_PROBE)?;
        if !self
            .approval_key
            .verify_strict(DIAGNOSTIC_SIGNING_PROBE, &signature)
        {
            return Err(HostStateRepositoryError::InvalidRecord {
                record: APPROVAL_KEY_RECORD,
            });
        }
        Ok(snapshot)
    }

    /// Returns the safe public reference to the retained approval key.
    #[must_use]
    pub fn approval_key_reference(&self) -> &ApprovalKeyReference {
        self.approval_key.reference()
    }

    /// Signs one B3-approved canonical payload without exposing the signing seed.
    pub(crate) fn sign_approval(
        &self,
        payload: &[u8],
    ) -> Result<HostApprovalSignature, HostStateRepositoryError> {
        self.approval_key.sign(payload)
    }

    /// Strictly verifies one signature using the retained approval public key.
    #[must_use]
    pub(crate) fn verify_approval(
        &self,
        payload: &[u8],
        signature: &HostApprovalSignature,
    ) -> bool {
        self.approval_key.verify_strict(payload, signature)
    }

    /// Loads the full internal governance state while retaining the repository lock.
    pub(crate) fn governance_state(&self) -> &GovernanceState {
        &self.governance
    }

    /// Replaces governance only if the supplied previous state remains current.
    pub(crate) fn replace_governance(
        &mut self,
        expected: &GovernanceState,
        replacement: GovernanceState,
    ) -> Result<(), HostStateRepositoryError> {
        if expected != &self.governance {
            return Err(HostStateRepositoryError::StaleGovernance);
        }
        validate_governance(
            &replacement,
            self.governance.status().host_id(),
            self.approval_key.reference(),
            &self.approval_key,
        )?;
        let encoded = replacement.encode()?;
        write_or_prove(&self.directory, GOVERNANCE_RECORD, &encoded)?;
        self.governance = replacement;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn inject_governance_directory_sync(&self, kind: std::io::ErrorKind) {
        self.directory.inject_directory_sync(kind);
    }

    #[cfg(test)]
    pub(crate) fn inject_governance_rename(&self, kind: std::io::ErrorKind) {
        self.directory.inject_rename(kind);
    }

    fn load_locked(
        directory: HostStateDir,
        lock: HostStateLock,
    ) -> Result<Self, HostStateRepositoryError> {
        let identity = read(&directory, IDENTITY_RECORD)?;
        let key = read_secret(&directory, APPROVAL_KEY_RECORD)?;
        let governance = read(&directory, GOVERNANCE_RECORD)?;
        let (identity, key, governance, finalize_bootstrap) = match (identity, key, governance) {
            (None, None, None) => {
                let identity = IdentityRecord::in_progress(generate_host_id()?);
                write_or_prove(&directory, IDENTITY_RECORD, &identity.encode()?)?;
                let key = ApprovalKeyRecord::generate(&identity.host_id)?;
                write_or_prove(&directory, APPROVAL_KEY_RECORD, &key.encode()?)?;
                let governance = GovernanceState::initial(
                    identity.host_id.clone(),
                    key.key.reference().clone(),
                )?;
                write_or_prove(&directory, GOVERNANCE_RECORD, &governance.encode()?)?;
                (identity, key, governance, true)
            }
            (Some(identity), key, governance) => {
                let identity = IdentityRecord::decode(&identity)?;
                match (identity.is_in_progress(), key, governance) {
                    (false, Some(key), Some(governance)) => (
                        identity,
                        ApprovalKeyRecord::decode(&key)?,
                        GovernanceState::decode(&governance)?,
                        false,
                    ),
                    (false, _, _) => return Err(HostStateRepositoryError::IncompleteRecordSet),
                    (true, None, None) => {
                        let key = ApprovalKeyRecord::generate(&identity.host_id)?;
                        write_or_prove(&directory, APPROVAL_KEY_RECORD, &key.encode()?)?;
                        let governance = GovernanceState::initial(
                            identity.host_id.clone(),
                            key.key.reference().clone(),
                        )?;
                        write_or_prove(&directory, GOVERNANCE_RECORD, &governance.encode()?)?;
                        (identity, key, governance, true)
                    }
                    (true, Some(key), None) => {
                        let key = ApprovalKeyRecord::decode(&key)?;
                        if key.host_id != identity.host_id {
                            return Err(HostStateRepositoryError::InvalidRecord {
                                record: APPROVAL_KEY_RECORD,
                            });
                        }
                        let governance = GovernanceState::initial(
                            identity.host_id.clone(),
                            key.key.reference().clone(),
                        )?;
                        write_or_prove(&directory, GOVERNANCE_RECORD, &governance.encode()?)?;
                        (identity, key, governance, true)
                    }
                    (true, Some(key), Some(governance)) => (
                        identity,
                        ApprovalKeyRecord::decode(&key)?,
                        GovernanceState::decode(&governance)?,
                        true,
                    ),
                    (true, None, Some(_)) => {
                        return Err(HostStateRepositoryError::IncompleteRecordSet);
                    }
                }
            }
            _ => return Err(HostStateRepositoryError::IncompleteRecordSet),
        };
        if key.host_id != identity.host_id {
            return Err(HostStateRepositoryError::InvalidRecord {
                record: APPROVAL_KEY_RECORD,
            });
        }
        validate_governance(
            &governance,
            &identity.host_id,
            key.key.reference(),
            &key.key,
        )?;
        if finalize_bootstrap {
            let complete = identity.complete_record();
            write_or_prove(&directory, IDENTITY_RECORD, &complete.encode()?)?;
        }
        Ok(Self {
            directory,
            _lock: lock,
            approval_key: key.key,
            governance,
        })
    }
}

fn acquire_lock(directory: &HostStateDir) -> Result<HostStateLock, HostStateRepositoryError> {
    for attempt in 0..=LOCK_RETRY_ATTEMPTS {
        match directory.acquire_lock() {
            Ok(lock) => return Ok(lock),
            Err(HostStateError::LockContended { .. }) if attempt < LOCK_RETRY_ATTEMPTS => {
                std::thread::sleep(LOCK_RETRY_DELAY);
            }
            Err(source) => {
                return Err(HostStateRepositoryError::Persistence {
                    record: pohunek_paths::HOST_STATE_LOCK_NAME,
                    source,
                });
            }
        }
    }
    unreachable!("lock retry range includes its terminal attempt")
}

fn read(
    directory: &HostStateDir,
    record: &'static str,
) -> Result<Option<Vec<u8>>, HostStateRepositoryError> {
    directory
        .read_record(record)
        .map_err(|source| HostStateRepositoryError::Persistence { record, source })
}

fn read_secret(
    directory: &HostStateDir,
    record: &'static str,
) -> Result<Option<Zeroizing<Vec<u8>>>, HostStateRepositoryError> {
    directory
        .read_secret_record(record)
        .map_err(|source| HostStateRepositoryError::Persistence { record, source })
}

fn write_or_prove(
    directory: &HostStateDir,
    record: &'static str,
    expected: &[u8],
) -> Result<(), HostStateRepositoryError> {
    match directory.replace_record(record, expected) {
        Ok(()) => Ok(()),
        Err(HostStateError::CommittedDurabilityUncertain { .. }) => {
            Err(HostStateRepositoryError::DurabilityUncertain { record })
        }
        Err(source) => Err(HostStateRepositoryError::Persistence { record, source }),
    }
}

fn validate_governance(
    governance: &GovernanceState,
    host_id: &HostId,
    approval_key_reference: &ApprovalKeyReference,
    key: &ApprovalKey,
) -> Result<(), HostStateRepositoryError> {
    let status = governance.status();
    if status.host_id() != host_id || status.approval_key_reference() != approval_key_reference {
        return Err(HostStateRepositoryError::InvalidRecord {
            record: GOVERNANCE_RECORD,
        });
    }
    if let Some(outcome) = governance.latest_transfer_outcome() {
        validate_outcome(outcome, status, approval_key_reference, key)?;
    }
    if let Some(retired) = governance.retired_enrollment() {
        validate_retired(retired, host_id, approval_key_reference, key)?;
    }
    Ok(())
}

fn validate_retired(
    retired: &RetiredEnrollment,
    host_id: &HostId,
    approval_key_reference: &ApprovalKeyReference,
    key: &ApprovalKey,
) -> Result<(), HostStateRepositoryError> {
    let status = HostGovernanceStatus::new(
        host_id.clone(),
        Some(retired.enrollment().clone()),
        Some(retired.owner().clone()),
        Some(retired.owner_revision()),
        None,
        approval_key_reference.clone(),
    )
    .map_err(|_error| HostStateRepositoryError::InvalidRecord {
        record: GOVERNANCE_RECORD,
    })?;
    if let Some(outcome) = retired.latest_transfer_outcome() {
        validate_outcome(outcome, &status, approval_key_reference, key)?;
    }
    Ok(())
}

fn validate_outcome(
    outcome: &SignedTransferOutcome,
    status: &HostGovernanceStatus,
    approval_key_reference: &ApprovalKeyReference,
    key: &ApprovalKey,
) -> Result<(), HostStateRepositoryError> {
    let candidate = outcome.candidate();
    let proposal = candidate.proposal();
    if proposal.host_id() != status.host_id()
        || candidate.approval_key_reference() != approval_key_reference
        || Some(candidate.new_owner_revision()) != status.owner_revision()
        || status.owner() != Some(proposal.target())
        || candidate.new_owner_revision()
            != proposal.owner_revision().checked_next().map_err(|_error| {
                HostStateRepositoryError::InvalidRecord {
                    record: GOVERNANCE_RECORD,
                }
            })?
    {
        return Err(HostStateRepositoryError::InvalidRecord {
            record: GOVERNANCE_RECORD,
        });
    }
    let Some(enrollment) = status.enrollment() else {
        return Err(HostStateRepositoryError::InvalidRecord {
            record: GOVERNANCE_RECORD,
        });
    };
    if enrollment.relay_id() != proposal.relay_id() {
        return Err(HostStateRepositoryError::InvalidRecord {
            record: GOVERNANCE_RECORD,
        });
    }
    let payload =
        outcome
            .canonical_payload()
            .map_err(|_error| HostStateRepositoryError::InvalidRecord {
                record: GOVERNANCE_RECORD,
            })?;
    if !key.verify_strict(&payload, outcome.signature()) {
        return Err(HostStateRepositoryError::InvalidRecord {
            record: GOVERNANCE_RECORD,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use base64::prelude::{Engine as _, BASE64_URL_SAFE_NO_PAD};
    use tempfile::TempDir;

    use super::*;
    use crate::host_state::approval_key::fail_next_entropy;

    fn state_dir() -> TempDir {
        let temp = tempfile::Builder::new()
            .prefix("pohunek-host-state-")
            .tempdir()
            .expect("create isolated host-state directory");
        std::fs::set_permissions(
            temp.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .expect("make isolated state directory owner-private");
        temp
    }

    fn records() -> (IdentityRecord, ApprovalKeyRecord, GovernanceState) {
        let identity =
            IdentityRecord::complete(generate_host_id().expect("generate host identity"));
        let key = ApprovalKeyRecord::generate(&identity.host_id).expect("generate approval key");
        let governance =
            GovernanceState::initial(identity.host_id.clone(), key.key.reference().clone())
                .expect("make initial governance");
        (identity, key, governance)
    }

    fn seed(path: &Path, include_identity: bool, include_key: bool, include_governance: bool) {
        let (identity, key, governance) = records();
        let identity = if include_identity && !(include_key && include_governance) {
            IdentityRecord::in_progress(identity.host_id.clone())
        } else {
            identity
        };
        let directory = HostStateDir::open_or_create(path).expect("open state directory");
        let _lock = directory.acquire_lock().expect("hold setup lock");
        if include_identity {
            directory
                .replace_record(
                    IDENTITY_RECORD,
                    &identity.encode().expect("encode identity"),
                )
                .expect("write identity");
        }
        if include_key {
            directory
                .replace_record(APPROVAL_KEY_RECORD, &key.encode().expect("encode key"))
                .expect("write approval key");
        }
        if include_governance {
            directory
                .replace_record(
                    GOVERNANCE_RECORD,
                    &governance.encode().expect("encode governance"),
                )
                .expect("write governance");
        }
    }

    fn identifier(prefix: &str, byte: u8) -> String {
        format!("{prefix}{}", BASE64_URL_SAFE_NO_PAD.encode([byte; 32]))
    }

    fn outcome_tamper_cases() -> Vec<(&'static str, &'static str, serde_json::Value)> {
        vec![
            (
                "host id",
                "candidate/proposal/host_id",
                serde_json::json!(identifier("host_", 12)),
            ),
            (
                "approval-key reference",
                "candidate/approval_key_reference",
                serde_json::json!(identifier("approval_key_", 13)),
            ),
            (
                "relay id",
                "candidate/proposal/relay_id",
                serde_json::json!(identifier("relay_", 14)),
            ),
            (
                "current owner value",
                "candidate/proposal/current_owner",
                serde_json::json!({"kind": "principal", "id": identifier("principal_", 15)}),
            ),
            (
                "current owner kind",
                "candidate/proposal/current_owner",
                serde_json::json!({"kind": "team", "id": identifier("team_", 16)}),
            ),
            (
                "target owner value",
                "candidate/proposal/target",
                serde_json::json!({"kind": "team", "id": identifier("team_", 17)}),
            ),
            (
                "target owner kind",
                "candidate/proposal/target",
                serde_json::json!({"kind": "principal", "id": identifier("principal_", 18)}),
            ),
            (
                "proposal owner revision",
                "candidate/proposal/owner_revision",
                serde_json::json!("3"),
            ),
            (
                "outcome owner revision",
                "candidate/new_owner_revision",
                serde_json::json!("3"),
            ),
            (
                "proposal id",
                "candidate/proposal/proposal_id",
                serde_json::json!(identifier("proposal_", 19)),
            ),
            (
                "outcome id",
                "candidate/outcome_id",
                serde_json::json!(identifier("outcome_", 20)),
            ),
            (
                "nonce",
                "candidate/proposal/nonce",
                serde_json::json!(identifier("nonce_", 21)),
            ),
            (
                "expiry",
                "candidate/proposal/expiry",
                serde_json::json!("2026-09-03T10:00:01Z"),
            ),
            (
                "suspension",
                "candidate/suspension",
                serde_json::json!("all_active_shares_tampered"),
            ),
            (
                "signature",
                "signature",
                serde_json::json!(format!(
                    "sig_{}",
                    BASE64_URL_SAFE_NO_PAD.encode([11_u8; 64])
                )),
            ),
        ]
    }

    fn assert_outcome_tamper_cases(
        state: &GovernanceState,
        outcome_path: &str,
        scope: &str,
        identity: &HostId,
        key: &ApprovalKeyRecord,
    ) {
        for (coordinate, tail, replacement) in outcome_tamper_cases() {
            let mut wire: serde_json::Value =
                serde_json::from_slice(&state.encode().expect("encode valid governance state"))
                    .expect("governance is JSON");
            *wire
                .pointer_mut(&format!("/{outcome_path}/{tail}"))
                .expect("tamper target exists") = replacement;
            let encoded = serde_json::to_vec(&wire).expect("encode tampered governance");
            let error = GovernanceState::decode(&encoded)
                .and_then(|tampered| {
                    validate_governance(&tampered, identity, key.key.reference(), &key.key)
                })
                .expect_err("tampered outcome must fail closed");
            assert!(matches!(
                error,
                HostStateRepositoryError::InvalidRecord {
                    record: GOVERNANCE_RECORD
                }
            ));
            assert!(
                !format!("{error:?} {error}").contains("B2-SECRET-SENTINEL-NEVER-RENDER!"),
                "{scope} {coordinate} error must redact secret material"
            );
        }
    }

    fn assert_retired_coordinates_remain_bound(
        state: &GovernanceState,
        identity: &HostId,
        key: &ApprovalKeyRecord,
    ) {
        for (coordinate, tail, replacement) in [
            (
                "retired relay",
                "enrollment/relay_id",
                serde_json::json!(identifier("relay_", 22)),
            ),
            (
                "retired owner",
                "owner",
                serde_json::json!({"kind": "principal", "id": identifier("principal_", 23)}),
            ),
            (
                "retired owner revision",
                "owner_revision",
                serde_json::json!("3"),
            ),
        ] {
            let mut wire: serde_json::Value =
                serde_json::from_slice(&state.encode().expect("encode retired governance"))
                    .expect("governance is JSON");
            *wire
                .pointer_mut(&format!("/retired_enrollment/{tail}"))
                .expect("retired coordinate exists") = replacement;
            let encoded = serde_json::to_vec(&wire).expect("encode tampered retired governance");
            assert!(
                matches!(
                    GovernanceState::decode(&encoded).and_then(|tampered| validate_governance(
                        &tampered,
                        identity,
                        key.key.reference(),
                        &key.key
                    )),
                    Err(HostStateRepositoryError::InvalidRecord {
                        record: GOVERNANCE_RECORD
                    })
                ),
                "{coordinate} must remain cross-bound to its retired outcome"
            );
        }
    }

    fn retired_governance_with_outcome(
        identity: &HostId,
        key: &ApprovalKeyRecord,
        candidate: &protocol::TransferOutcomeCandidate,
        outcome: protocol::SignedTransferOutcome,
        target: protocol::HostOwner,
    ) -> GovernanceState {
        let retired = RetiredEnrollment::new(
            protocol::EnrollmentInfo::new(
                candidate.proposal().relay_id().clone(),
                protocol::EnrollmentStatus::LocallyUnenrolled,
                protocol::EnrollmentRevision::new(1).expect("retired enrollment revision"),
            ),
            target.clone(),
            protocol::OwnerRevision::new(2).expect("retired owner revision"),
            Some(outcome),
        )
        .expect("valid retired summary");
        GovernanceState::new(
            protocol::HostGovernanceStatus::new(
                identity.clone(),
                Some(protocol::EnrollmentInfo::new(
                    candidate.proposal().relay_id().clone(),
                    protocol::EnrollmentStatus::PendingLocalCommit,
                    protocol::EnrollmentRevision::new(2).expect("current enrollment revision"),
                )),
                Some(target),
                Some(protocol::OwnerRevision::new(3).expect("current owner revision")),
                None,
                key.key.reference().clone(),
            )
            .expect("valid current state following retired enrollment"),
            None,
            Some(retired),
        )
        .expect("valid retired governance")
    }

    #[test]
    fn bootstrap_only_repairs_the_three_safe_prefixes() {
        for (identity, key, governance) in [
            (false, false, false),
            (true, false, false),
            (true, true, false),
            (true, true, true),
        ] {
            let temp = state_dir();
            seed(temp.path(), identity, key, governance);
            let repository = HostStateRepository::open_or_create(temp.path())
                .expect("safe prefix must bootstrap or reload");
            let snapshot = repository.snapshot();
            assert_eq!(snapshot.host_id(), snapshot.governance().host_id());
            assert_eq!(
                repository.approval_key_reference(),
                snapshot.governance().approval_key_reference()
            );
        }

        for (identity, key, governance) in [
            (false, true, false),
            (false, false, true),
            (true, false, true),
            (false, true, true),
        ] {
            let temp = state_dir();
            seed(temp.path(), identity, key, governance);
            assert!(matches!(
                HostStateRepository::open_or_create(temp.path()),
                Err(HostStateRepositoryError::IncompleteRecordSet)
            ));
        }
    }

    #[test]
    fn restart_preserves_identity_and_approval_reference() {
        let temp = state_dir();
        let first = HostStateRepository::open_or_create(temp.path()).expect("bootstrap repository");
        let snapshot = first.snapshot();
        let reference = first.approval_key_reference().clone();
        drop(first);
        let second = HostStateRepository::open_or_create(temp.path()).expect("reload repository");
        assert_eq!(second.snapshot(), snapshot);
        assert_eq!(second.approval_key_reference(), &reference);
    }

    #[test]
    fn diagnostic_revalidates_the_reopened_stable_snapshot() {
        let temp = state_dir();
        let repository = HostStateRepository::open_or_create(temp.path()).expect("bootstrap");

        assert_eq!(
            repository.diagnose().expect("diagnostic"),
            repository.snapshot()
        );
    }

    #[test]
    fn diagnostic_rejects_missing_unsafe_and_swapped_records() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let temp = state_dir();
        let repository = HostStateRepository::open_or_create(temp.path()).expect("bootstrap");
        let directory = repository.directory.path().to_path_buf();
        std::fs::remove_file(directory.join(GOVERNANCE_RECORD)).expect("remove governance");
        assert!(matches!(
            repository.diagnose(),
            Err(HostStateRepositoryError::IncompleteRecordSet)
        ));
        drop(repository);

        let temp = state_dir();
        let repository = HostStateRepository::open_or_create(temp.path()).expect("bootstrap");
        let identity = repository.directory.path().join(IDENTITY_RECORD);
        std::fs::set_permissions(&identity, std::fs::Permissions::from_mode(0o644))
            .expect("make identity unsafe");
        assert!(matches!(
            repository.diagnose(),
            Err(HostStateRepositoryError::Persistence {
                record: IDENTITY_RECORD,
                ..
            })
        ));
        drop(repository);

        let temp = state_dir();
        let repository = HostStateRepository::open_or_create(temp.path()).expect("bootstrap");
        let directory = repository.directory.path().to_path_buf();
        let governance = directory.join(GOVERNANCE_RECORD);
        std::fs::remove_file(&governance).expect("remove governance for link injection");
        symlink(directory.join(IDENTITY_RECORD), &governance).expect("inject governance symlink");
        assert!(matches!(
            repository.diagnose(),
            Err(HostStateRepositoryError::Persistence {
                record: GOVERNANCE_RECORD,
                ..
            })
        ));
    }

    #[test]
    fn diagnostic_rejects_a_current_host_directory_replacement_without_repair() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = state_dir();
        let repository = HostStateRepository::open_or_create(temp.path()).expect("bootstrap");
        let host = repository.directory.path().to_path_buf();
        let displaced = temp.path().join("displaced-host");
        std::fs::rename(&host, &displaced).expect("displace retained host directory");
        std::fs::create_dir(&host).expect("replace current host directory");
        std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o700))
            .expect("make replacement owner-private");

        assert!(matches!(
            repository.diagnose(),
            Err(HostStateRepositoryError::Persistence {
                record: "host directory",
                ..
            })
        ));
        assert!(
            std::fs::read_dir(&host)
                .expect("read replacement directory")
                .next()
                .is_none(),
            "diagnostic must not repair a replaced current host directory"
        );
    }

    #[test]
    fn malformed_duplicate_and_future_records_fail_closed() {
        let temp = state_dir();
        let identity = generate_host_id().expect("host identity");
        let directory = HostStateDir::open_or_create(temp.path()).expect("open state directory");
        let lock = directory.acquire_lock().expect("hold setup lock");
        let duplicate = format!(
            r#"{{"format_version":2,"format_version":2,"host_id":"{}","phase":"complete"}}"#,
            identity.as_str()
        );
        directory
            .replace_record(IDENTITY_RECORD, duplicate.as_bytes())
            .expect("write duplicate identity");
        drop(lock);
        assert!(matches!(
            HostStateRepository::open_or_create(temp.path()),
            Err(HostStateRepositoryError::InvalidRecord {
                record: IDENTITY_RECORD
            })
        ));

        let temp = state_dir();
        let directory = HostStateDir::open_or_create(temp.path()).expect("open state directory");
        let lock = directory.acquire_lock().expect("hold setup lock");
        let future = format!(
            r#"{{"format_version":3,"host_id":"{}","phase":"complete"}}"#,
            identity.as_str()
        );
        directory
            .replace_record(IDENTITY_RECORD, future.as_bytes())
            .expect("write future identity");
        drop(lock);
        assert!(matches!(
            HostStateRepository::open_or_create(temp.path()),
            Err(HostStateRepositoryError::UnsupportedRecordVersion {
                record: IDENTITY_RECORD
            })
        ));
    }

    #[test]
    fn identity_phase_is_strict_and_complete_records_never_repair_missing_peers() {
        let identity = generate_host_id().expect("host identity");
        for malformed in [
            format!(
                r#"{{"format_version":2,"host_id":"{}"}}"#,
                identity.as_str()
            ),
            format!(
                r#"{{"format_version":2,"host_id":"{}","phase":"unknown"}}"#,
                identity.as_str()
            ),
            format!(
                r#"{{"format_version":2,"host_id":"{}","phase":"complete","extra":true}}"#,
                identity.as_str()
            ),
            format!(
                r#"{{"format_version":2,"host_id":"{}","phase":"complete","phase":"in_progress"}}"#,
                identity.as_str()
            ),
        ] {
            let temp = state_dir();
            let directory =
                HostStateDir::open_or_create(temp.path()).expect("open state directory");
            let lock = directory.acquire_lock().expect("hold setup lock");
            directory
                .replace_record(IDENTITY_RECORD, malformed.as_bytes())
                .expect("write malformed identity");
            drop(lock);
            assert!(matches!(
                HostStateRepository::open_or_create(temp.path()),
                Err(HostStateRepositoryError::InvalidRecord {
                    record: IDENTITY_RECORD
                })
            ));
        }

        for (include_key, include_governance) in [(false, false), (true, false), (false, true)] {
            let temp = state_dir();
            let (identity, key, governance) = records();
            let directory =
                HostStateDir::open_or_create(temp.path()).expect("open state directory");
            let lock = directory.acquire_lock().expect("hold setup lock");
            directory
                .replace_record(
                    IDENTITY_RECORD,
                    &identity.encode().expect("encode complete identity"),
                )
                .expect("write complete identity");
            if include_key {
                directory
                    .replace_record(
                        APPROVAL_KEY_RECORD,
                        &key.encode().expect("encode approval key"),
                    )
                    .expect("write approval key");
            }
            if include_governance {
                directory
                    .replace_record(
                        GOVERNANCE_RECORD,
                        &governance.encode().expect("encode governance"),
                    )
                    .expect("write governance");
            }
            drop(lock);
            assert!(matches!(
                HostStateRepository::open_or_create(temp.path()),
                Err(HostStateRepositoryError::IncompleteRecordSet)
            ));
        }
    }

    #[test]
    fn invalid_binary_key_and_cross_binding_fail_closed() {
        let temp = state_dir();
        let (identity, _key, governance) = records();
        let directory = HostStateDir::open_or_create(temp.path()).expect("open state directory");
        let lock = directory.acquire_lock().expect("hold setup lock");
        directory
            .replace_record(
                IDENTITY_RECORD,
                &identity.encode().expect("encode identity"),
            )
            .expect("write identity");
        directory
            .replace_record(APPROVAL_KEY_RECORD, b"PHAPKEY\0")
            .expect("write truncated key");
        directory
            .replace_record(
                GOVERNANCE_RECORD,
                &governance.encode().expect("encode governance"),
            )
            .expect("write governance");
        drop(lock);
        assert!(matches!(
            HostStateRepository::open_or_create(temp.path()),
            Err(HostStateRepositoryError::InvalidRecord { .. })
        ));

        let temp = state_dir();
        let (identity, _key, governance) = records();
        let other = ApprovalKeyRecord::generate(&generate_host_id().expect("other host"))
            .expect("other key");
        let directory = HostStateDir::open_or_create(temp.path()).expect("open state directory");
        let lock = directory.acquire_lock().expect("hold setup lock");
        directory
            .replace_record(
                IDENTITY_RECORD,
                &identity.encode().expect("encode identity"),
            )
            .expect("write identity");
        directory
            .replace_record(
                APPROVAL_KEY_RECORD,
                &other.encode().expect("encode other key"),
            )
            .expect("write other key");
        directory
            .replace_record(
                GOVERNANCE_RECORD,
                &governance.encode().expect("encode governance"),
            )
            .expect("write governance");
        drop(lock);
        assert!(matches!(
            HostStateRepository::open_or_create(temp.path()),
            Err(HostStateRepositoryError::InvalidRecord { .. })
        ));
    }

    #[test]
    fn entropy_failures_do_not_create_partial_records() {
        let temp = state_dir();
        fail_next_entropy();
        assert!(matches!(
            HostStateRepository::open_or_create(temp.path()),
            Err(HostStateRepositoryError::Entropy {
                stage: "host identity"
            })
        ));

        let temp = state_dir();
        seed(temp.path(), true, false, false);
        fail_next_entropy();
        assert!(matches!(
            HostStateRepository::open_or_create(temp.path()),
            Err(HostStateRepositoryError::Entropy {
                stage: "approval key"
            })
        ));
    }

    #[test]
    fn signer_and_errors_redact_private_material() {
        let seed = *b"B2-SECRET-SENTINEL-NEVER-RENDER!";
        let key = ApprovalKey::from_test_seed(&seed).expect("construct signer");
        let rendered = format!("{key:?}");
        assert!(!rendered.contains("B2-SECRET-SENTINEL-NEVER-RENDER!"));

        let temp = state_dir();
        let repository = HostStateRepository::open_or_create(temp.path()).expect("bootstrap");
        let signature = repository
            .sign_approval(b"approval payload")
            .expect("sign payload");
        assert!(repository.verify_approval(b"approval payload", &signature));
        assert!(!repository.verify_approval(b"different payload", &signature));
        let error = HostStateRepositoryError::InvalidRecord {
            record: "approval.key",
        };
        assert!(!format!("{error:?} {error}").contains("B2-SECRET-SENTINEL-NEVER-RENDER!"));
    }

    #[test]
    fn retired_state_must_be_a_single_locally_unenrolled_summary() {
        let (identity, key, governance) = records();
        let active = protocol::EnrollmentInfo::new(
            protocol::RelayId::parse("relay_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
                .expect("relay"),
            protocol::EnrollmentStatus::Active,
            protocol::EnrollmentRevision::new(1).expect("revision"),
        );
        RetiredEnrollment::new(
            active,
            protocol::HostOwner::Principal(
                protocol::PrincipalId::parse(
                    "principal_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                )
                .expect("principal"),
            ),
            protocol::OwnerRevision::new(1).expect("revision"),
            None,
        )
        .expect_err("active enrollment cannot become retired");
        assert_eq!(governance.status().host_id(), &identity.host_id);
        assert_eq!(key.host_id, identity.host_id);
    }

    #[test]
    fn stale_expected_state_cannot_replace_governance() {
        let temp = state_dir();
        let mut repository = HostStateRepository::open_or_create(temp.path()).expect("bootstrap");
        let stale = GovernanceState::initial(
            generate_host_id().expect("different host"),
            repository.approval_key_reference().clone(),
        )
        .expect("make distinct state");
        let replacement = repository.governance_state().clone();
        assert!(matches!(
            repository.replace_governance(&stale, replacement),
            Err(HostStateRepositoryError::StaleGovernance)
        ));
    }

    #[test]
    fn durability_uncertainty_on_identity_stops_bootstrap_until_a_new_open() {
        let temp = state_dir();
        let directory = HostStateDir::open_or_create(temp.path()).expect("open state directory");
        directory.inject_directory_sync(std::io::ErrorKind::Other);
        let lock = directory.acquire_lock().expect("hold state lock");

        assert!(matches!(
            HostStateRepository::load_locked(directory, lock),
            Err(HostStateRepositoryError::DurabilityUncertain {
                record: IDENTITY_RECORD
            })
        ));

        let directory = HostStateDir::open_or_create(temp.path()).expect("reopen state directory");
        let identity = directory
            .read_record(IDENTITY_RECORD)
            .expect("read committed identity")
            .expect("identity remains visible after rename");
        assert!(
            directory
                .read_secret_record(APPROVAL_KEY_RECORD)
                .expect("read approval key")
                .is_none(),
            "bootstrap must not continue to the approval key"
        );
        assert!(
            directory
                .read_record(GOVERNANCE_RECORD)
                .expect("read governance")
                .is_none(),
            "bootstrap must not continue to governance"
        );

        let repository = HostStateRepository::open_or_create(temp.path())
            .expect("a new open explicitly reloads and completes the valid prefix");
        assert_eq!(
            repository.snapshot().host_id(),
            &IdentityRecord::decode(&identity)
                .expect("decode retained identity")
                .host_id
        );
    }

    #[test]
    fn durability_uncertainty_on_approval_key_stops_before_governance() {
        let temp = state_dir();
        seed(temp.path(), true, false, false);
        let directory = HostStateDir::open_or_create(temp.path()).expect("open state directory");
        directory.inject_directory_sync(std::io::ErrorKind::Other);
        let lock = directory.acquire_lock().expect("hold state lock");

        assert!(matches!(
            HostStateRepository::load_locked(directory, lock),
            Err(HostStateRepositoryError::DurabilityUncertain {
                record: APPROVAL_KEY_RECORD
            })
        ));

        let directory = HostStateDir::open_or_create(temp.path()).expect("reopen state directory");
        assert!(directory
            .read_record(IDENTITY_RECORD)
            .expect("read identity")
            .is_some());
        let key = directory
            .read_secret_record(APPROVAL_KEY_RECORD)
            .expect("read committed approval key")
            .expect("approval key remains visible after rename");
        assert!(
            directory
                .read_record(GOVERNANCE_RECORD)
                .expect("read governance")
                .is_none(),
            "bootstrap must not continue to governance"
        );

        let repository = HostStateRepository::open_or_create(temp.path())
            .expect("a new open explicitly reloads and completes the valid prefix");
        assert_eq!(
            repository.approval_key_reference(),
            ApprovalKeyRecord::decode(&key)
                .expect("decode retained approval key")
                .key
                .reference()
        );
    }

    #[test]
    fn durability_uncertainty_on_initial_governance_never_returns_a_snapshot() {
        let temp = state_dir();
        seed(temp.path(), true, true, false);
        let directory = HostStateDir::open_or_create(temp.path()).expect("open state directory");
        directory.inject_directory_sync(std::io::ErrorKind::Other);
        let lock = directory.acquire_lock().expect("hold state lock");

        assert!(matches!(
            HostStateRepository::load_locked(directory, lock),
            Err(HostStateRepositoryError::DurabilityUncertain {
                record: GOVERNANCE_RECORD
            })
        ));

        let directory = HostStateDir::open_or_create(temp.path()).expect("reopen state directory");
        let governance = directory
            .read_record(GOVERNANCE_RECORD)
            .expect("read committed governance")
            .expect("governance remains visible after rename");
        let expected = GovernanceState::decode(&governance).expect("decode retained governance");

        let repository = HostStateRepository::open_or_create(temp.path())
            .expect("a new open explicitly reloads the valid record set");
        assert_eq!(repository.snapshot().governance(), expected.status());
    }

    #[test]
    fn durability_uncertainty_on_bootstrap_finalization_never_publishes_a_repository() {
        let temp = state_dir();
        let (identity, key, governance) = records();
        let directory = HostStateDir::open_or_create(temp.path()).expect("open state directory");
        let lock = directory.acquire_lock().expect("hold setup lock");
        let in_progress = IdentityRecord::in_progress(identity.host_id.clone());
        directory
            .replace_record(
                IDENTITY_RECORD,
                &in_progress.encode().expect("encode in-progress identity"),
            )
            .expect("write in-progress identity");
        directory
            .replace_record(
                APPROVAL_KEY_RECORD,
                &key.encode().expect("encode approval key"),
            )
            .expect("write approval key");
        directory
            .replace_record(
                GOVERNANCE_RECORD,
                &governance.encode().expect("encode governance"),
            )
            .expect("write governance");
        directory.inject_directory_sync(std::io::ErrorKind::Other);

        assert!(matches!(
            HostStateRepository::load_locked(directory, lock),
            Err(HostStateRepositoryError::DurabilityUncertain {
                record: IDENTITY_RECORD
            })
        ));

        let repository = HostStateRepository::open_or_create(temp.path())
            .expect("a fresh open explicitly revalidates the completed set");
        assert_eq!(repository.snapshot().host_id(), &identity.host_id);
    }

    #[test]
    fn durability_uncertainty_on_governance_replacement_keeps_snapshot_unpublished() {
        let temp = state_dir();
        let mut repository = HostStateRepository::open_or_create(temp.path()).expect("bootstrap");
        let previous = repository.governance_state().clone();
        let snapshot = repository.snapshot();
        let relay = protocol::RelayId::parse("relay_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
            .expect("relay");
        let owner = protocol::HostOwner::Principal(
            protocol::PrincipalId::parse("principal_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
                .expect("owner"),
        );
        let replacement = GovernanceState::with_quarantine_origin(
            HostGovernanceStatus::new(
                snapshot.host_id().clone(),
                Some(protocol::EnrollmentInfo::new(
                    relay,
                    protocol::EnrollmentStatus::Quarantined,
                    protocol::EnrollmentRevision::new(1).expect("enrollment revision"),
                )),
                Some(owner),
                Some(protocol::OwnerRevision::new(1).expect("owner revision")),
                Some(protocol::QuarantineReason::ProjectionConflict),
                repository.approval_key_reference().clone(),
            )
            .expect("valid replacement status"),
            Some(protocol::EnrollmentStatus::Active),
            None,
            None,
        )
        .expect("valid replacement governance");
        repository
            .directory
            .inject_directory_sync(std::io::ErrorKind::Other);

        assert!(matches!(
            repository.replace_governance(&previous, replacement.clone()),
            Err(HostStateRepositoryError::DurabilityUncertain {
                record: GOVERNANCE_RECORD
            })
        ));
        assert_eq!(
            repository.snapshot(),
            snapshot,
            "the uncertain replacement must not update the in-memory snapshot"
        );
        drop(repository);

        let reopened = HostStateRepository::open_or_create(temp.path())
            .expect("a new open explicitly reloads the visible replacement");
        assert_eq!(reopened.governance_state(), &replacement);
    }

    #[test]
    fn signed_outcome_must_bind_current_coordinates_and_verify_strictly() {
        let identity = generate_host_id().expect("host identity");
        let key = ApprovalKeyRecord::generate(&identity).expect("approval key");
        let relay = protocol::RelayId::parse(&identifier("relay_", 1)).expect("relay");
        let current_owner = protocol::HostOwner::Principal(
            protocol::PrincipalId::parse(&identifier("principal_", 2)).expect("principal"),
        );
        let target = protocol::HostOwner::Team(
            protocol::TeamId::parse(&identifier("team_", 3)).expect("team"),
        );
        let enrollment = protocol::EnrollmentInfo::new(
            relay.clone(),
            protocol::EnrollmentStatus::Active,
            protocol::EnrollmentRevision::new(1).expect("enrollment revision"),
        );
        let proposal = protocol::TransferProposal::new(
            protocol::TransferCoordinates::new(
                relay,
                identity.clone(),
                protocol::OwnerRevision::new(1).expect("proposal revision"),
                current_owner,
                target.clone(),
            ),
            protocol::ProposalId::parse(&identifier("proposal_", 4)).expect("proposal"),
            protocol::ProposalNonce::parse(&identifier("nonce_", 5)).expect("nonce"),
            protocol::ProposalExpiry::parse("2026-09-03T10:00:00Z").expect("expiry"),
        );
        let candidate = protocol::TransferOutcomeCandidate::new(
            protocol::TransferOutcomeId::from_proposal_id(proposal.proposal_id()),
            proposal,
            protocol::OwnerRevision::new(2).expect("outcome revision"),
            protocol::ShareSuspensionIntent::AllActiveShares,
            key.key.reference().clone(),
        )
        .expect("advance revision");
        let payload = candidate.canonical_payload().expect("canonical payload");
        let outcome = protocol::SignedTransferOutcome::new(
            candidate.clone(),
            key.key.sign(&payload).expect("sign outcome"),
        );
        let status = protocol::HostGovernanceStatus::new(
            identity.clone(),
            Some(enrollment),
            Some(target.clone()),
            Some(protocol::OwnerRevision::new(2).expect("owner revision")),
            None,
            key.key.reference().clone(),
        )
        .expect("valid current governance");
        let governance =
            GovernanceState::new(status, Some(outcome.clone()), None).expect("governance");
        validate_governance(&governance, &identity, key.key.reference(), &key.key)
            .expect("valid signed outcome");

        let retired_governance = retired_governance_with_outcome(
            &identity,
            &key,
            &candidate,
            outcome.clone(),
            target.clone(),
        );
        validate_governance(
            &retired_governance,
            &identity,
            key.key.reference(),
            &key.key,
        )
        .expect("valid retired signed outcome");

        assert_outcome_tamper_cases(
            &governance,
            "latest_transfer_outcome",
            "current",
            &identity,
            &key,
        );
        assert_outcome_tamper_cases(
            &retired_governance,
            "retired_enrollment/latest_transfer_outcome",
            "retired",
            &identity,
            &key,
        );
        assert_retired_coordinates_remain_bound(&retired_governance, &identity, &key);

        let bad_outcome = protocol::SignedTransferOutcome::new(
            candidate,
            key.key
                .sign(b"wrong signing domain")
                .expect("sign wrong payload"),
        );
        let bad = GovernanceState::new(governance.status().clone(), Some(bad_outcome), None)
            .expect("well-formed but unverifiable outcome");
        assert!(validate_governance(&bad, &identity, key.key.reference(), &key.key).is_err());
    }
}
