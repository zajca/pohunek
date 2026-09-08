//! Durable host-governance coordination.
//!
//! This C1 module owns the daemon-wide lease of stable host state and exposes
//! only immutable safe snapshots. It deliberately exposes no protocol handler.

// Rust guideline compliant 2026-09-04

use std::fmt::{Debug, Formatter};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use protocol::{
    EnrollmentInfo, EnrollmentStatus, HostGovernanceStatus, HostId, HostOwner, OwnerRevision,
    QuarantineReason, RelayId, ShareSuspensionIntent, SignedTransferOutcome,
    TransferOutcomeCandidate, TransferOutcomeId, TransferProposal,
};
use time::OffsetDateTime;
use tokio::sync::Mutex as AsyncMutex;

use crate::host_state::records::{GovernanceState, RetiredEnrollment};
use crate::host_state::{HostStateRepository, HostStateRepositoryError, HostStateSnapshot};

/// Daemon-owned access to the locked durable host-governance repository.
#[derive(Clone)]
pub struct HostGovernanceService {
    inner: Arc<Inner>,
}

#[expect(
    clippy::too_many_lines,
    reason = "The complete lifecycle transition matrix is intentionally kept contiguous for security audit."
)]
fn apply_lifecycle_command(
    state: &GovernanceState,
    command: LocalGovernanceCommand,
) -> Result<GovernanceState, HostGovernanceError> {
    match command {
        LocalGovernanceCommand::Initial { relay_id, owner } => {
            if state.status().enrollment().is_some() {
                return Err(GovernanceTransitionError::InvalidLifecycle.into());
            }
            let status = new_status(
                state.status(),
                Some(EnrollmentInfo::new(
                    relay_id,
                    EnrollmentStatus::PendingLocalCommit,
                    protocol::EnrollmentRevision::new(1)
                        .map_err(|_error| GovernanceTransitionError::RevisionOverflow)?,
                )),
                Some(owner),
                Some(
                    OwnerRevision::new(1)
                        .map_err(|_error| GovernanceTransitionError::RevisionOverflow)?,
                ),
                None,
            )?;
            new_state(status, None, None)
        }
        LocalGovernanceCommand::Activate { expected } => {
            require_expected(state, &expected)?;
            let (enrollment, owner, owner_revision) = current_coordinates(state)?;
            match enrollment.status() {
                EnrollmentStatus::PendingLocalCommit | EnrollmentStatus::Rotating => {}
                EnrollmentStatus::Disabled
                    if expected.enrollment() == state.status().enrollment() => {}
                EnrollmentStatus::Quarantined => {
                    return Err(GovernanceTransitionError::InvalidLifecycle.into())
                }
                _ => return Err(GovernanceTransitionError::InvalidLifecycle.into()),
            }
            let status = new_status(
                state.status(),
                Some(EnrollmentInfo::new(
                    enrollment.relay_id().clone(),
                    EnrollmentStatus::Active,
                    next_enrollment_revision(&enrollment)?,
                )),
                Some(owner),
                Some(owner_revision),
                None,
            )?;
            new_state(
                status,
                state.latest_transfer_outcome().cloned(),
                state.retired_enrollment().cloned(),
            )
        }
        LocalGovernanceCommand::Rotate { expected } => {
            require_expected(state, &expected)?;
            let (enrollment, owner, owner_revision) = current_coordinates(state)?;
            if enrollment.status() != EnrollmentStatus::Active {
                return Err(GovernanceTransitionError::InvalidLifecycle.into());
            }
            let status = new_status(
                state.status(),
                Some(EnrollmentInfo::new(
                    enrollment.relay_id().clone(),
                    EnrollmentStatus::Rotating,
                    next_enrollment_revision(&enrollment)?,
                )),
                Some(owner),
                Some(owner_revision),
                None,
            )?;
            new_state(
                status,
                state.latest_transfer_outcome().cloned(),
                state.retired_enrollment().cloned(),
            )
        }
        LocalGovernanceCommand::Disable { expected } => {
            require_expected(state, &expected)?;
            let (enrollment, owner, owner_revision) = current_coordinates(state)?;
            if !matches!(
                enrollment.status(),
                EnrollmentStatus::PendingLocalCommit
                    | EnrollmentStatus::Active
                    | EnrollmentStatus::Rotating
            ) {
                return Err(GovernanceTransitionError::InvalidLifecycle.into());
            }
            let status = new_status(
                state.status(),
                Some(EnrollmentInfo::new(
                    enrollment.relay_id().clone(),
                    EnrollmentStatus::Disabled,
                    next_enrollment_revision(&enrollment)?,
                )),
                Some(owner),
                Some(owner_revision),
                None,
            )?;
            new_state(
                status,
                state.latest_transfer_outcome().cloned(),
                state.retired_enrollment().cloned(),
            )
        }
        LocalGovernanceCommand::LocallyUnenroll { expected } => {
            require_expected(state, &expected)?;
            let (enrollment, owner, owner_revision) = current_coordinates(state)?;
            if !matches!(
                enrollment.status(),
                EnrollmentStatus::PendingLocalCommit
                    | EnrollmentStatus::Rotating
                    | EnrollmentStatus::Active
                    | EnrollmentStatus::Disabled
                    | EnrollmentStatus::Quarantined
            ) {
                return Err(GovernanceTransitionError::InvalidLifecycle.into());
            }
            let status = new_status(
                state.status(),
                Some(EnrollmentInfo::new(
                    enrollment.relay_id().clone(),
                    EnrollmentStatus::LocallyUnenrolled,
                    next_enrollment_revision(&enrollment)?,
                )),
                Some(owner),
                Some(owner_revision),
                None,
            )?;
            new_state(
                status,
                state.latest_transfer_outcome().cloned(),
                state.retired_enrollment().cloned(),
            )
        }
        LocalGovernanceCommand::Fresh {
            expected,
            relay_id,
            owner,
        } => {
            require_expected(state, &expected)?;
            let (enrollment, current_owner, owner_revision) = current_coordinates(state)?;
            if enrollment.status() != EnrollmentStatus::LocallyUnenrolled {
                return Err(GovernanceTransitionError::InvalidLifecycle.into());
            }
            let retired = RetiredEnrollment::new(
                enrollment.clone(),
                current_owner,
                owner_revision,
                state.latest_transfer_outcome().cloned(),
            )
            .map_err(HostGovernanceError::Persistence)?;
            let status = new_status(
                state.status(),
                Some(EnrollmentInfo::new(
                    relay_id,
                    EnrollmentStatus::PendingLocalCommit,
                    next_enrollment_revision(&enrollment)?,
                )),
                Some(owner),
                Some(
                    owner_revision
                        .checked_next()
                        .map_err(|_error| GovernanceTransitionError::RevisionOverflow)?,
                ),
                None,
            )?;
            new_state(status, None, Some(retired))
        }
        LocalGovernanceCommand::Transfer { .. } => {
            unreachable!("transfer is handled with signer access")
        }
    }
}

fn apply_transfer(
    repository: &mut HostStateRepository,
    expected: &GovernanceState,
    proposal: TransferProposal,
    now: OffsetDateTime,
) -> Result<Option<SignedTransferOutcome>, HostGovernanceError> {
    if let Some(outcome) = durable_transfer_retry(repository, expected, &proposal)? {
        return Ok(Some(outcome));
    }
    let (enrollment, owner, owner_revision) = current_coordinates(expected)?;
    if enrollment.status() != EnrollmentStatus::Active
        || expected.status().quarantine().is_some()
        || proposal.relay_id() != enrollment.relay_id()
        || proposal.host_id() != expected.status().host_id()
        || proposal.current_owner() != &owner
        || proposal.owner_revision() != owner_revision
    {
        return Err(GovernanceTransitionError::ProposalMismatch.into());
    }
    if proposal.expiry().is_expired_at(now) {
        return Err(GovernanceTransitionError::ProposalExpired.into());
    }
    let next_owner_revision = owner_revision
        .checked_next()
        .map_err(|_error| GovernanceTransitionError::RevisionOverflow)?;
    let candidate = TransferOutcomeCandidate::new(
        TransferOutcomeId::from_proposal_id(proposal.proposal_id()),
        proposal,
        next_owner_revision,
        ShareSuspensionIntent::AllActiveShares,
        repository.approval_key_reference().clone(),
    )
    .map_err(|_error| GovernanceTransitionError::ProposalMismatch)?;
    let payload = candidate
        .canonical_payload()
        .map_err(|_error| GovernanceTransitionError::ProposalMismatch)?;
    let signature = repository
        .sign_approval(&payload)
        .map_err(HostGovernanceError::Persistence)?;
    if !repository.verify_approval(&payload, &signature) {
        return Err(GovernanceTransitionError::ProjectionRejected.into());
    }
    let outcome = SignedTransferOutcome::new(candidate, signature);
    let status = new_status(
        expected.status(),
        Some(enrollment),
        Some(outcome.candidate().proposal().target().clone()),
        Some(next_owner_revision),
        None,
    )?;
    let replacement = new_state(
        status,
        Some(outcome.clone()),
        expected.retired_enrollment().cloned(),
    )?;
    repository
        .replace_governance(expected, replacement)
        .map_err(HostGovernanceError::Persistence)?;
    Ok(Some(outcome))
}

fn require_expected(
    state: &GovernanceState,
    expected: &HostGovernanceStatus,
) -> Result<(), HostGovernanceError> {
    if state.status() == expected {
        Ok(())
    } else {
        Err(GovernanceTransitionError::StaleCoordinates.into())
    }
}

fn current_coordinates(
    state: &GovernanceState,
) -> Result<(EnrollmentInfo, HostOwner, OwnerRevision), HostGovernanceError> {
    let status = state.status();
    match (status.enrollment(), status.owner(), status.owner_revision()) {
        (Some(enrollment), Some(owner), Some(owner_revision)) => {
            Ok((enrollment.clone(), owner.clone(), owner_revision))
        }
        _ => Err(GovernanceTransitionError::RelayIneligible.into()),
    }
}

fn next_enrollment_revision(
    enrollment: &EnrollmentInfo,
) -> Result<protocol::EnrollmentRevision, HostGovernanceError> {
    enrollment
        .revision()
        .checked_next()
        .map_err(|_error| GovernanceTransitionError::RevisionOverflow.into())
}

fn new_status(
    current: &HostGovernanceStatus,
    enrollment: Option<EnrollmentInfo>,
    owner: Option<HostOwner>,
    owner_revision: Option<OwnerRevision>,
    quarantine: Option<QuarantineReason>,
) -> Result<HostGovernanceStatus, HostGovernanceError> {
    HostGovernanceStatus::new(
        current.host_id().clone(),
        enrollment,
        owner,
        owner_revision,
        quarantine,
        current.approval_key_reference().clone(),
    )
    .map_err(|_error| GovernanceTransitionError::InvalidLifecycle.into())
}

fn new_state(
    status: HostGovernanceStatus,
    outcome: Option<SignedTransferOutcome>,
    retired: Option<RetiredEnrollment>,
) -> Result<GovernanceState, HostGovernanceError> {
    GovernanceState::new(status, outcome, retired).map_err(HostGovernanceError::Persistence)
}

fn quarantined_state(
    state: &GovernanceState,
    reason: QuarantineReason,
) -> Result<GovernanceState, HostGovernanceError> {
    let (enrollment, owner, owner_revision) = current_coordinates(state)?;
    if enrollment.status() == EnrollmentStatus::Quarantined {
        return if state.status().quarantine() == Some(reason) {
            Ok(state.clone())
        } else {
            Err(GovernanceTransitionError::InvalidLifecycle.into())
        };
    }
    let status = new_status(
        state.status(),
        Some(EnrollmentInfo::new(
            enrollment.relay_id().clone(),
            EnrollmentStatus::Quarantined,
            next_enrollment_revision(&enrollment)?,
        )),
        Some(owner),
        Some(owner_revision),
        Some(reason),
    )?;
    GovernanceState::with_quarantine_origin(
        status,
        Some(enrollment.status()),
        state.latest_transfer_outcome().cloned(),
        state.retired_enrollment().cloned(),
    )
    .map_err(HostGovernanceError::Persistence)
}

fn restore_projection_conflict(
    state: &GovernanceState,
) -> Result<GovernanceState, HostGovernanceError> {
    let (enrollment, owner, owner_revision) = current_coordinates(state)?;
    let origin = state
        .quarantine_origin()
        .ok_or(GovernanceTransitionError::InvalidLifecycle)?;
    if enrollment.status() != EnrollmentStatus::Quarantined
        || state.status().quarantine() != Some(QuarantineReason::ProjectionConflict)
        || origin == EnrollmentStatus::Quarantined
    {
        return Err(GovernanceTransitionError::InvalidLifecycle.into());
    }
    let status = new_status(
        state.status(),
        Some(EnrollmentInfo::new(
            enrollment.relay_id().clone(),
            origin,
            next_enrollment_revision(&enrollment)?,
        )),
        Some(owner),
        Some(owner_revision),
        None,
    )?;
    new_state(
        status,
        state.latest_transfer_outcome().cloned(),
        state.retired_enrollment().cloned(),
    )
}

fn durable_transfer_retry(
    repository: &HostStateRepository,
    state: &GovernanceState,
    proposal: &TransferProposal,
) -> Result<Option<SignedTransferOutcome>, HostGovernanceError> {
    let current = state.latest_transfer_outcome();
    let retired = state
        .retired_enrollment()
        .and_then(RetiredEnrollment::latest_transfer_outcome);
    for outcome in current.into_iter().chain(retired) {
        verify_outcome(repository, outcome)?;
        let stored = outcome.candidate().proposal();
        if stored == proposal {
            return Ok(Some(outcome.clone()));
        }
        if stored.proposal_id() == proposal.proposal_id() || stored.has_same_nonce(proposal) {
            return Err(GovernanceTransitionError::ProposalReplay.into());
        }
    }
    Ok(None)
}

fn verify_outcome(
    repository: &HostStateRepository,
    outcome: &SignedTransferOutcome,
) -> Result<(), HostGovernanceError> {
    let payload = outcome
        .canonical_payload()
        .map_err(|_error| GovernanceTransitionError::ProjectionRejected)?;
    if repository.verify_approval(&payload, outcome.signature()) {
        Ok(())
    } else {
        Err(GovernanceTransitionError::ProjectionRejected.into())
    }
}

struct Inner {
    state_root: PathBuf,
    repository: Mutex<Option<HostStateRepository>>,
    reload: AsyncMutex<()>,
    #[cfg(test)]
    test_root: Option<tempfile::TempDir>,
    #[cfg(test)]
    reload_repository_lock_attempt: Mutex<Option<ReloadRepositoryLockAttempt>>,
}

#[cfg(test)]
struct ReloadRepositoryLockAttempt {
    attempted: tokio::sync::oneshot::Sender<()>,
}

impl Debug for HostGovernanceService {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostGovernanceService")
            .field("state", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

/// Failure while operating daemon-owned host governance.
#[derive(Debug, thiserror::Error)]
pub enum HostGovernanceError {
    /// Opening or updating durable host state failed.
    #[error("host governance persistence failed")]
    Persistence(#[source] HostStateRepositoryError),
    /// The blocking startup task did not complete.
    #[error("host governance startup task did not complete")]
    StartupTask,
    /// A blocking durable-state mutation task did not complete.
    #[error("host governance mutation task did not complete")]
    MutationTask,
    /// A requested local-governance transition was unsafe or stale.
    #[error("host governance transition was rejected")]
    Transition(#[from] GovernanceTransitionError),
    /// The retained repository is intentionally unavailable until explicit reload.
    #[error("host governance requires explicit durable-state reload")]
    ReloadRequired,
    /// A service mutex was poisoned by a prior panic.
    #[error("host governance state is unavailable")]
    Unavailable,
}

/// Redacted result of revalidating durable host governance.
///
/// This crate-private diagnostic carries only the existing safe public
/// governance status. Persistence errors, paths, record bytes, signatures,
/// and signer material are intentionally collapsed into fixed states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostGovernanceDiagnostic {
    /// All current durable records revalidated against the retained snapshot.
    Available(HostGovernanceStatus),
    /// A prior commit had uncertain durability and requires explicit reload.
    ReloadRequired,
    /// Durable records or their filesystem bindings failed safe validation.
    IntegrityFailed,
    /// The service could not acquire its retained repository safely.
    Unavailable,
}

/// Fixed-text errors for durable local governance transitions.
#[derive(Debug, thiserror::Error)]
pub enum GovernanceTransitionError {
    /// The current durable coordinates did not match the confirmed command.
    #[error("confirmed governance coordinates are stale")]
    StaleCoordinates,
    /// The requested lifecycle move is not valid from the durable state.
    #[error("governance lifecycle transition is not permitted")]
    InvalidLifecycle,
    /// A relay-sensitive operation requires one healthy active enrollment.
    #[error("relay governance is not currently eligible")]
    RelayIneligible,
    /// A proposal bound one of the wrong durable coordinates.
    #[error("transfer proposal does not match durable governance")]
    ProposalMismatch,
    /// A proposal expired before durable local confirmation.
    #[error("transfer proposal has expired")]
    ProposalExpired,
    /// A proposal attempted to reuse durable replay coordinates.
    #[error("transfer proposal replay was rejected")]
    ProposalReplay,
    /// A signed outcome or projection failed local verification.
    #[error("host governance projection was rejected")]
    ProjectionRejected,
    /// A checked durable revision could not advance.
    #[error("host governance revision cannot advance")]
    RevisionOverflow,
}

/// Move-only same-UID confirmation of one complete internal governance command.
pub(crate) struct LocalGovernanceConfirmation {
    command: LocalGovernanceCommand,
}

impl Debug for LocalGovernanceConfirmation {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalGovernanceConfirmation([REDACTED])")
    }
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Only a future crate-private same-UID adapter may create these complete commands."
    )
)]
enum LocalGovernanceCommand {
    Initial {
        relay_id: RelayId,
        owner: HostOwner,
    },
    Activate {
        expected: HostGovernanceStatus,
    },
    Rotate {
        expected: HostGovernanceStatus,
    },
    Disable {
        expected: HostGovernanceStatus,
    },
    LocallyUnenroll {
        expected: HostGovernanceStatus,
    },
    Fresh {
        expected: HostGovernanceStatus,
        relay_id: RelayId,
        owner: HostOwner,
    },
    Transfer {
        proposal: TransferProposal,
    },
}

/// Relay-side projection supplied only by a future host-link adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Only a future crate-private relay reconciliation adapter supplies projections."
    )
)]
pub(crate) enum RelayProjection {
    /// The relay reports no durable outcome.
    Missing,
    /// The relay reports one durable signed outcome.
    Outcome(Box<SignedTransferOutcome>),
}

impl HostGovernanceService {
    #[cfg(test)]
    pub(crate) fn open_test() -> Self {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::Builder::new()
            .prefix("pohunek-governance-test-")
            .tempdir()
            .expect("create isolated host-governance root");
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
            .expect("make host-governance root owner-private");
        let state_root = root.path().to_path_buf();
        Self {
            inner: Arc::new(Inner {
                repository: Mutex::new(Some(
                    HostStateRepository::open_or_create(&state_root)
                        .expect("open isolated real host-state repository"),
                )),
                reload: AsyncMutex::new(()),
                state_root,
                test_root: Some(root),
                reload_repository_lock_attempt: Mutex::new(None),
            }),
        }
    }

    /// Opens durable host governance before daemon subsystems start.
    ///
    /// `state_root` is the canonical application state directory, not its `host` child.
    ///
    /// # Errors
    ///
    /// Returns an error when secure bootstrap or the blocking startup task fails.
    pub async fn open(state_root: PathBuf) -> Result<Self, HostGovernanceError> {
        let root = state_root.clone();
        let repository =
            tokio::task::spawn_blocking(move || HostStateRepository::open_or_create(root))
                .await
                .map_err(|_error| HostGovernanceError::StartupTask)?
                .map_err(HostGovernanceError::Persistence)?;
        Ok(Self {
            inner: Arc::new(Inner {
                state_root,
                repository: Mutex::new(Some(repository)),
                reload: AsyncMutex::new(()),
                #[cfg(test)]
                test_root: None,
                #[cfg(test)]
                reload_repository_lock_attempt: Mutex::new(None),
            }),
        })
    }

    /// Returns an immutable safe snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`HostGovernanceError::ReloadRequired`] after uncertain durability.
    pub fn snapshot(&self) -> Result<HostStateSnapshot, HostGovernanceError> {
        let slot = self
            .inner
            .repository
            .lock()
            .map_err(|_error| HostGovernanceError::Unavailable)?;
        let repository = slot.as_ref().ok_or(HostGovernanceError::ReloadRequired)?;
        Ok(repository.snapshot())
    }

    /// Returns the stable host identity.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::snapshot`].
    pub fn host_id(&self) -> Result<HostId, HostGovernanceError> {
        Ok(self.snapshot()?.host_id().clone())
    }

    /// Returns the current public governance state.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::snapshot`].
    pub fn status(&self) -> Result<HostGovernanceStatus, HostGovernanceError> {
        Ok(self.snapshot()?.governance().clone())
    }

    /// Returns a safe governance snapshot without blocking an async caller.
    ///
    /// The repository mutex is acquired only on Tokio's blocking pool. Public
    /// request handlers must use this method rather than [`Self::status`].
    ///
    /// # Errors
    ///
    /// Returns the same service errors as [`Self::status`].
    pub(crate) async fn inspect(&self) -> Result<HostGovernanceStatus, HostGovernanceError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let slot = inner
                .repository
                .lock()
                .map_err(|_error| HostGovernanceError::Unavailable)?;
            let repository = slot.as_ref().ok_or(HostGovernanceError::ReloadRequired)?;
            Ok(repository.snapshot().governance().clone())
        })
        .await
        .map_err(|_error| HostGovernanceError::StartupTask)?
    }

    /// Revalidates durable host governance through the service's blocking boundary.
    ///
    /// This read-only diagnostic never returns filesystem paths, raw record
    /// bytes, signatures, proposals, nonces, or signer material.
    pub(crate) async fn diagnose(&self) -> HostGovernanceDiagnostic {
        let inner = Arc::clone(&self.inner);
        match tokio::task::spawn_blocking(move || {
            let slot = match inner.repository.lock() {
                Ok(slot) => slot,
                Err(_poisoned) => return HostGovernanceDiagnostic::Unavailable,
            };
            let Some(repository) = slot.as_ref() else {
                return HostGovernanceDiagnostic::ReloadRequired;
            };
            match repository.diagnose() {
                Ok(snapshot) => HostGovernanceDiagnostic::Available(snapshot.governance().clone()),
                Err(_error) => HostGovernanceDiagnostic::IntegrityFailed,
            }
        })
        .await
        {
            Ok(diagnostic) => diagnostic,
            Err(_error) => HostGovernanceDiagnostic::Unavailable,
        }
    }

    /// Applies one move-only locally confirmed durable governance command.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Only a future crate-private same-UID adapter invokes durable local confirmation."
        )
    )]
    pub(crate) async fn apply_local_confirmation(
        &self,
        confirmation: LocalGovernanceConfirmation,
    ) -> Result<Option<SignedTransferOutcome>, HostGovernanceError> {
        self.apply_local_confirmation_at(confirmation, OffsetDateTime::now_utc())
            .await
    }

    async fn apply_local_confirmation_at(
        &self,
        confirmation: LocalGovernanceConfirmation,
        now: OffsetDateTime,
    ) -> Result<Option<SignedTransferOutcome>, HostGovernanceError> {
        self.mutate_locked(move |repository| {
            let expected = repository.governance_state().clone();
            match confirmation.command {
                LocalGovernanceCommand::Transfer { proposal } => {
                    apply_transfer(repository, &expected, proposal, now)
                }
                command => {
                    let replacement = apply_lifecycle_command(&expected, command)?;
                    repository
                        .replace_governance(&expected, replacement)
                        .map_err(HostGovernanceError::Persistence)?;
                    Ok(None)
                }
            }
        })
        .await
    }

    /// Returns one already committed exact transfer retry without a new confirmation.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Only a future crate-private relay adapter asks for an exact durable retry."
        )
    )]
    pub(crate) async fn retry_transfer(
        &self,
        proposal: TransferProposal,
    ) -> Result<SignedTransferOutcome, HostGovernanceError> {
        self.mutate_locked(move |repository| {
            let state = repository.governance_state();
            durable_transfer_retry(repository, state, &proposal)?
                .ok_or_else(|| GovernanceTransitionError::ProposalMismatch.into())
        })
        .await
    }

    /// Reconciles a future relay projection against the authoritative durable outcome.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Only a future crate-private relay adapter supplies its semantic projection."
        )
    )]
    pub(crate) async fn reconcile_projection(
        &self,
        projection: RelayProjection,
    ) -> Result<(), HostGovernanceError> {
        self.mutate_locked(move |repository| {
            let expected = repository.governance_state().clone();
            let healthy = match (expected.latest_transfer_outcome(), &projection) {
                (None, RelayProjection::Missing) => true,
                (Some(current), RelayProjection::Outcome(observed)) => {
                    verify_outcome(repository, current).is_ok()
                        && verify_outcome(repository, observed.as_ref()).is_ok()
                        && current == observed.as_ref()
                }
                _ => false,
            };
            if expected
                .status()
                .enrollment()
                .is_some_and(|enrollment| enrollment.status() == EnrollmentStatus::Quarantined)
            {
                if healthy
                    && expected.status().quarantine() == Some(QuarantineReason::ProjectionConflict)
                {
                    let replacement = restore_projection_conflict(&expected)?;
                    repository
                        .replace_governance(&expected, replacement)
                        .map_err(HostGovernanceError::Persistence)?;
                }
                return Ok(());
            }
            if healthy {
                return Ok(());
            }
            let replacement = quarantined_state(&expected, QuarantineReason::ProjectionConflict)?;
            repository
                .replace_governance(&expected, replacement)
                .map_err(HostGovernanceError::Persistence)
        })
        .await
    }

    /// Persists a detected local clone or enrollment conflict without touching sessions.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Only a future crate-private detector reports a clone or enrollment conflict."
        )
    )]
    pub(crate) async fn quarantine(
        &self,
        expected_status: HostGovernanceStatus,
        reason: QuarantineReason,
    ) -> Result<(), HostGovernanceError> {
        self.mutate_locked(move |repository| {
            let expected = repository.governance_state().clone();
            require_expected(&expected, &expected_status)?;
            let replacement = quarantined_state(&expected, reason)?;
            repository
                .replace_governance(&expected, replacement)
                .map_err(HostGovernanceError::Persistence)
        })
        .await
    }

    /// Reloads state only after a prior uncertain durability result.
    ///
    /// # Errors
    ///
    /// Returns an error when the reread cannot establish one valid record set.
    pub async fn reload(&self) -> Result<(), HostGovernanceError> {
        let _reload = self.inner.reload.lock().await;
        let inner = Arc::clone(&self.inner);
        let loaded = tokio::task::spawn_blocking(move || -> Result<bool, HostGovernanceError> {
            #[cfg(test)]
            notify_reload_repository_lock_attempt(&inner);
            let slot = inner
                .repository
                .lock()
                .map_err(|_error| HostGovernanceError::Unavailable)?;
            Ok(slot.is_some())
        })
        .await
        .map_err(|_error| HostGovernanceError::StartupTask)??;
        if loaded {
            return Ok(());
        }
        let root = self.inner.state_root.clone();
        let repository =
            tokio::task::spawn_blocking(move || HostStateRepository::open_or_create(root))
                .await
                .map_err(|_error| HostGovernanceError::StartupTask)?
                .map_err(HostGovernanceError::Persistence)?;
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || -> Result<(), HostGovernanceError> {
            let mut slot = inner
                .repository
                .lock()
                .map_err(|_error| HostGovernanceError::Unavailable)?;
            if slot.is_none() {
                *slot = Some(repository);
            }
            Ok(())
        })
        .await
        .map_err(|_error| HostGovernanceError::StartupTask)?
    }

    pub(crate) async fn mutate_locked<T, F>(&self, action: F) -> Result<T, HostGovernanceError>
    where
        T: Send + 'static,
        F: FnOnce(&mut HostStateRepository) -> Result<T, HostGovernanceError> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut slot = inner
                .repository
                .lock()
                .map_err(|_error| HostGovernanceError::Unavailable)?;
            let repository = slot.as_mut().ok_or(HostGovernanceError::ReloadRequired)?;
            match action(repository) {
                Err(
                    error @ HostGovernanceError::Persistence(
                        HostStateRepositoryError::DurabilityUncertain { .. },
                    ),
                ) => {
                    let _discarded = slot.take();
                    Err(error)
                }
                Err(error) => Err(error),
                Ok(value) => Ok(value),
            }
        })
        .await
        .map_err(|_error| HostGovernanceError::MutationTask)?
    }

    #[cfg(test)]
    fn install_reload_repository_lock_attempt(&self, attempted: tokio::sync::oneshot::Sender<()>) {
        *self
            .inner
            .reload_repository_lock_attempt
            .lock()
            .expect("reload repository lock test hook") =
            Some(ReloadRepositoryLockAttempt { attempted });
    }

    #[cfg(test)]
    async fn force_governance_durability_uncertainty(&self) -> Result<(), HostGovernanceError> {
        self.mutate_locked(|repository| {
            repository.inject_governance_directory_sync(std::io::ErrorKind::Other);
            let current = repository.governance_state().clone();
            repository
                .replace_governance(&current.clone(), current)
                .map_err(HostGovernanceError::Persistence)
        })
        .await
    }

    #[cfg(test)]
    async fn inject_governance_rename_failure(&self) -> Result<(), HostGovernanceError> {
        self.mutate_locked(|repository| {
            repository.inject_governance_rename(std::io::ErrorKind::ReadOnlyFilesystem);
            Ok(())
        })
        .await
    }

    #[cfg(test)]
    async fn inject_governance_directory_sync_failure(&self) -> Result<(), HostGovernanceError> {
        self.mutate_locked(|repository| {
            repository.inject_governance_directory_sync(std::io::ErrorKind::Other);
            Ok(())
        })
        .await
    }

    #[cfg(test)]
    fn test_state_root(&self) -> &std::path::Path {
        self.inner
            .test_root
            .as_ref()
            .expect("test service owns an isolated temporary root")
            .path()
    }
}

const fn assert_send<T: Send>() {}
const _: () = assert_send::<HostStateRepository>();
const _: () = assert_send::<HostGovernanceService>();
const _: () = assert_send::<Arc<HostGovernanceService>>();

#[cfg(test)]
fn notify_reload_repository_lock_attempt(inner: &Inner) {
    let attempt = inner
        .reload_repository_lock_attempt
        .lock()
        .expect("reload repository lock test hook")
        .take();
    if let Some(attempt) = attempt {
        attempt
            .attempted
            .send(())
            .expect("reload responsiveness test waits for the lock attempt");
    }
}

#[cfg(test)]
mod owner_path_tests;

#[cfg(test)]
mod tests {
    use base64::prelude::{Engine as _, BASE64_URL_SAFE_NO_PAD};
    use protocol::{
        EnrollmentStatus, HostGovernanceStatus, HostId, HostOwner, OwnerRevision, QuarantineReason,
        RelayId, SignedTransferOutcome, TransferProposal,
    };
    use std::os::unix::fs::PermissionsExt as _;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Duration;
    use time::OffsetDateTime;

    use super::{
        HostGovernanceDiagnostic, HostGovernanceError, HostGovernanceService,
        LocalGovernanceCommand, LocalGovernanceConfirmation, RelayProjection,
    };
    use crate::host_state::records::GovernanceState;
    use crate::host_state::HostStateRepositoryError;

    /// Bounds a synchronization assertion without relying on arbitrary sleeps.
    const BLOCKED_MUTATION_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(1);
    /// Bounds proof that unrelated work keeps progressing while durable state is locked.
    const RUNTIME_RESPONSIVENESS_TIMEOUT: Duration = Duration::from_secs(1);
    /// Releases a test-held repository mutex if a synchronous regression stalls Tokio.
    const RELOAD_LOCK_WATCHDOG_TIMEOUT: Duration = Duration::from_secs(1);

    fn identifier(prefix: &str, byte: u8) -> String {
        format!("{prefix}{}", BASE64_URL_SAFE_NO_PAD.encode([byte; 32]))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn inspect_keeps_the_current_thread_runtime_responsive_while_repository_is_busy() {
        let service = Arc::new(HostGovernanceService::open_test());
        let expected = service.status().expect("initial safe status");
        let (entered_send, entered_recv) = tokio::sync::oneshot::channel();
        let (release_send, release_recv) = std::sync::mpsc::channel();
        let inner = Arc::clone(&service.inner);
        let holder = tokio::task::spawn_blocking(move || {
            let _guard = inner.repository.lock().expect("repository mutex");
            entered_send.send(()).expect("signal mutex acquisition");
            release_recv.recv().expect("release mutex holder");
        });
        entered_recv.await.expect("repository mutex is held");

        let inspect = tokio::spawn({
            let service = Arc::clone(&service);
            async move { service.inspect().await }
        });
        tokio::task::yield_now().await;
        let (progressed_send, progressed_recv) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            progressed_send.send(()).expect("unrelated task progresses");
        });
        tokio::time::timeout(Duration::from_secs(1), progressed_recv)
            .await
            .expect("unrelated task must progress while inspection waits")
            .expect("unrelated task completion");

        release_send.send(()).expect("release repository mutex");
        holder.await.expect("join holder");
        assert_eq!(
            inspect.await.expect("join inspection").expect("inspect"),
            expected
        );
    }

    fn relay(byte: u8) -> RelayId {
        RelayId::parse(&identifier("relay_", byte)).expect("valid relay")
    }

    fn principal(byte: u8) -> HostOwner {
        HostOwner::Principal(
            protocol::PrincipalId::parse(&identifier("principal_", byte)).expect("valid principal"),
        )
    }

    fn confirmation(command: LocalGovernanceCommand) -> LocalGovernanceConfirmation {
        LocalGovernanceConfirmation { command }
    }

    fn transfer(
        status: &HostGovernanceStatus,
        proposal_byte: u8,
        nonce_byte: u8,
        target: HostOwner,
    ) -> TransferProposal {
        let enrollment = status.enrollment().expect("enrolled host");
        TransferProposal::new(
            protocol::TransferCoordinates::new(
                enrollment.relay_id().clone(),
                status.host_id().clone(),
                status.owner_revision().expect("owner revision"),
                status.owner().expect("owner").clone(),
                target,
            ),
            protocol::ProposalId::parse(&identifier("proposal_", proposal_byte))
                .expect("valid proposal id"),
            protocol::ProposalNonce::parse(&identifier("nonce_", nonce_byte)).expect("valid nonce"),
            protocol::ProposalExpiry::parse("2030-01-01T00:00:00Z").expect("future expiry"),
        )
    }

    async fn activate_new_host(service: &HostGovernanceService) -> HostGovernanceStatus {
        service
            .apply_local_confirmation(confirmation(LocalGovernanceCommand::Initial {
                relay_id: relay(1),
                owner: principal(2),
            }))
            .await
            .expect("initial confirmed enrollment");
        let pending = service.status().expect("pending status");
        service
            .apply_local_confirmation(confirmation(LocalGovernanceCommand::Activate {
                expected: pending,
            }))
            .await
            .expect("activate enrollment");
        service.status().expect("active status")
    }

    async fn establish_lifecycle(
        service: &HostGovernanceService,
        lifecycle: EnrollmentStatus,
    ) -> HostGovernanceStatus {
        service
            .apply_local_confirmation(confirmation(LocalGovernanceCommand::Initial {
                relay_id: relay(1),
                owner: principal(2),
            }))
            .await
            .expect("initial confirmed enrollment");
        let pending = service.status().expect("pending status");
        match lifecycle {
            EnrollmentStatus::PendingLocalCommit => pending,
            EnrollmentStatus::Active => {
                service
                    .apply_local_confirmation(confirmation(LocalGovernanceCommand::Activate {
                        expected: pending,
                    }))
                    .await
                    .expect("activate enrollment");
                service.status().expect("active status")
            }
            EnrollmentStatus::Rotating => {
                service
                    .apply_local_confirmation(confirmation(LocalGovernanceCommand::Activate {
                        expected: pending,
                    }))
                    .await
                    .expect("activate enrollment");
                let active = service.status().expect("active status");
                service
                    .apply_local_confirmation(confirmation(LocalGovernanceCommand::Rotate {
                        expected: active,
                    }))
                    .await
                    .expect("start rotation");
                service.status().expect("rotating status")
            }
            EnrollmentStatus::Disabled => {
                service
                    .apply_local_confirmation(confirmation(LocalGovernanceCommand::Activate {
                        expected: pending,
                    }))
                    .await
                    .expect("activate enrollment");
                let active = service.status().expect("active status");
                service
                    .apply_local_confirmation(confirmation(LocalGovernanceCommand::Disable {
                        expected: active,
                    }))
                    .await
                    .expect("disable enrollment");
                service.status().expect("disabled status")
            }
            EnrollmentStatus::LocallyUnenrolled => {
                service
                    .apply_local_confirmation(confirmation(LocalGovernanceCommand::Activate {
                        expected: pending,
                    }))
                    .await
                    .expect("activate enrollment");
                let active = service.status().expect("active status");
                service
                    .apply_local_confirmation(confirmation(
                        LocalGovernanceCommand::LocallyUnenroll { expected: active },
                    ))
                    .await
                    .expect("locally unenroll enrollment");
                service.status().expect("locally unenrolled status")
            }
            EnrollmentStatus::Quarantined => {
                panic!("quarantine is constructed only by the explicit detector path")
            }
        }
    }

    async fn open_service_at(root: &Path) -> HostGovernanceService {
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
            .expect("make isolated application state root owner-private");
        HostGovernanceService::open(root.to_path_buf())
            .await
            .expect("open isolated durable governance service")
    }

    fn transfer_with_expiry(
        status: &HostGovernanceStatus,
        proposal_byte: u8,
        nonce_byte: u8,
        target: HostOwner,
        expiry: &str,
    ) -> TransferProposal {
        let enrollment = status.enrollment().expect("enrolled host");
        TransferProposal::new(
            protocol::TransferCoordinates::new(
                enrollment.relay_id().clone(),
                status.host_id().clone(),
                status.owner_revision().expect("owner revision"),
                status.owner().expect("owner").clone(),
                target,
            ),
            protocol::ProposalId::parse(&identifier("proposal_", proposal_byte))
                .expect("valid proposal id"),
            protocol::ProposalNonce::parse(&identifier("nonce_", nonce_byte)).expect("valid nonce"),
            protocol::ProposalExpiry::parse(expiry).expect("canonical expiry"),
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "The adversarial table names each signed transfer coordinate independently."
    )]
    fn transfer_with_coordinates(
        relay_id: RelayId,
        host_id: HostId,
        owner_revision: OwnerRevision,
        current_owner: HostOwner,
        target: HostOwner,
        proposal_byte: u8,
        nonce_byte: u8,
        expiry: &str,
    ) -> TransferProposal {
        TransferProposal::new(
            protocol::TransferCoordinates::new(
                relay_id,
                host_id,
                owner_revision,
                current_owner,
                target,
            ),
            protocol::ProposalId::parse(&identifier("proposal_", proposal_byte))
                .expect("valid proposal id"),
            protocol::ProposalNonce::parse(&identifier("nonce_", nonce_byte)).expect("valid nonce"),
            protocol::ProposalExpiry::parse(expiry).expect("canonical expiry"),
        )
    }

    fn team(byte: u8) -> HostOwner {
        HostOwner::Team(protocol::TeamId::parse(&identifier("team_", byte)).expect("valid team"))
    }

    fn at(value: &str) -> OffsetDateTime {
        OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
            .expect("canonical test time")
    }

    #[tokio::test]
    async fn service_opens_and_shares_an_immutable_snapshot() {
        let service = Arc::new(HostGovernanceService::open_test());
        let shared = Arc::clone(&service);

        assert!(Arc::ptr_eq(&service, &shared));
        assert_eq!(
            service.snapshot().expect("initial snapshot"),
            shared.snapshot().expect("shared snapshot")
        );
    }

    #[test]
    fn test_service_temp_root_is_removed_after_the_last_service_arc_drops() {
        let service = Arc::new(HostGovernanceService::open_test());
        let retained = Arc::clone(&service);
        let root = service.test_state_root().to_path_buf();
        let approval_key = root
            .join(pohunek_paths::HOST_STATE_SUBDIR)
            .join(pohunek_paths::HOST_APPROVAL_KEY_NAME);

        assert!(approval_key.is_file());
        assert!(!format!("{service:?}").contains(&root.display().to_string()));

        drop(service);

        assert!(
            root.exists(),
            "retained service clone keeps test state alive"
        );
        assert!(
            approval_key.is_file(),
            "retained service clone keeps approval key alive"
        );

        drop(retained);

        assert!(!root.exists(), "test-owned host state is removed on drop");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_mutations_run_off_runtime_and_serialize_repository_access() {
        let service = HostGovernanceService::open_test();
        let runtime_thread = std::thread::current().id();
        let (first_started_tx, first_started_rx) = mpsc::sync_channel(1);
        let (release_first_tx, release_first_rx) = mpsc::sync_channel(1);
        let (second_started_tx, second_started_rx) = mpsc::sync_channel(1);
        let second_started_rx = Arc::new(Mutex::new(second_started_rx));

        let first_service = service.clone();
        let first = tokio::spawn(async move {
            first_service
                .mutate_locked(move |repository| {
                    first_started_tx
                        .send(std::thread::current().id())
                        .expect("report first mutation worker");
                    release_first_rx.recv().expect("release first mutation");
                    let current = repository.governance_state().clone();
                    repository
                        .replace_governance(&current.clone(), current)
                        .map_err(HostGovernanceError::Persistence)
                })
                .await
        });

        let worker_thread = tokio::task::spawn_blocking(move || {
            first_started_rx
                .recv_timeout(BLOCKED_MUTATION_OBSERVATION_TIMEOUT)
                .expect("first mutation starts")
        })
        .await
        .expect("first mutation observation task joins");
        assert_ne!(worker_thread, runtime_thread);

        let second_service = service.clone();
        let second = tokio::spawn(async move {
            second_service
                .mutate_locked(move |repository| {
                    second_started_tx
                        .send(())
                        .expect("report second mutation worker");
                    let current = repository.governance_state().clone();
                    repository
                        .replace_governance(&current.clone(), current)
                        .map_err(HostGovernanceError::Persistence)
                })
                .await
        });

        let blocked_receiver = Arc::clone(&second_started_rx);
        let blocked = tokio::task::spawn_blocking(move || {
            blocked_receiver
                .lock()
                .expect("lock second-start receiver")
                .recv_timeout(BLOCKED_MUTATION_OBSERVATION_TIMEOUT)
        })
        .await
        .expect("blocked mutation observation task joins");
        assert!(matches!(blocked, Err(mpsc::RecvTimeoutError::Timeout)));

        release_first_tx.send(()).expect("release first mutation");
        first
            .await
            .expect("first mutation task joins")
            .expect("first mutation persists");
        second
            .await
            .expect("second mutation task joins")
            .expect("second mutation persists");
        second_started_rx
            .lock()
            .expect("lock second-start receiver")
            .recv_timeout(BLOCKED_MUTATION_OBSERVATION_TIMEOUT)
            .expect("second mutation starts after first releases repository");
    }

    #[tokio::test]
    async fn blocking_mutation_join_failure_is_typed_and_redacted() {
        let service = HostGovernanceService::open_test();
        let result = service
            .mutate_locked(|_repository| -> Result<(), HostGovernanceError> {
                panic!("intentional test-only mutation task failure");
            })
            .await;

        assert!(matches!(&result, Err(HostGovernanceError::MutationTask)));
        assert_eq!(
            result.expect_err("mutation task failure").to_string(),
            "host governance mutation task did not complete"
        );
        assert!(matches!(
            service.snapshot(),
            Err(HostGovernanceError::Unavailable)
        ));
    }

    #[tokio::test]
    async fn service_uses_the_application_state_root_once() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().expect("create isolated application state root");
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
            .expect("make isolated application state root owner-private");
        let service = HostGovernanceService::open(root.path().to_path_buf())
            .await
            .expect("open service from application state root");

        assert!(root
            .path()
            .join(pohunek_paths::HOST_STATE_SUBDIR)
            .join(pohunek_paths::HOST_IDENTITY_NAME)
            .is_file());
        assert!(!root
            .path()
            .join(pohunek_paths::HOST_STATE_SUBDIR)
            .join(pohunek_paths::HOST_STATE_SUBDIR)
            .exists());
        assert!(!format!("{service:?}").contains(&root.path().display().to_string()));
    }

    #[test]
    fn poisoned_repository_mutex_is_unavailable() {
        let service = HostGovernanceService::open_test();
        let inner = Arc::clone(&service.inner);

        let _ = catch_unwind(AssertUnwindSafe(|| {
            let _guard = inner.repository.lock().expect("lock repository");
            panic!("intentional test-only mutex poison");
        }));

        assert!(matches!(
            service.snapshot(),
            Err(HostGovernanceError::Unavailable)
        ));
    }

    #[tokio::test]
    async fn uncertain_durability_discards_repository_until_explicit_reload() {
        let service = HostGovernanceService::open_test();
        let expected = service.snapshot().expect("initial snapshot");

        assert!(matches!(
            service.force_governance_durability_uncertainty().await,
            Err(HostGovernanceError::Persistence(
                HostStateRepositoryError::DurabilityUncertain {
                    record: "governance.json"
                }
            ))
        ));
        assert!(matches!(
            service.snapshot(),
            Err(HostGovernanceError::ReloadRequired)
        ));

        let (first_reload, second_reload) = tokio::join!(service.reload(), service.reload());
        first_reload.expect("first explicit reload succeeds");
        second_reload.expect("concurrent explicit reload observes the restored repository");
        assert_eq!(service.snapshot().expect("reloaded snapshot"), expected);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reload_keeps_the_current_thread_runtime_responsive_while_repository_is_busy() {
        let service = Arc::new(HostGovernanceService::open_test());
        assert!(matches!(
            service.force_governance_durability_uncertainty().await,
            Err(HostGovernanceError::Persistence(
                HostStateRepositoryError::DurabilityUncertain {
                    record: "governance.json"
                }
            ))
        ));

        let (holder_entered_send, holder_entered_recv) = tokio::sync::oneshot::channel();
        let (reload_attempt_send, reload_attempt_recv) = tokio::sync::oneshot::channel();
        service.install_reload_repository_lock_attempt(reload_attempt_send);
        let (release_send, release_recv) = std::sync::mpsc::channel();
        let inner = Arc::clone(&service.inner);
        let holder = tokio::task::spawn_blocking(move || {
            let _guard = inner.repository.lock().expect("repository mutex");
            holder_entered_send
                .send(())
                .expect("signal mutex acquisition");
            release_recv
                .recv_timeout(RELOAD_LOCK_WATCHDOG_TIMEOUT)
                .expect("bounded release of repository mutex holder");
        });
        holder_entered_recv.await.expect("repository mutex is held");

        let (watchdog_complete_send, watchdog_complete_recv) = mpsc::channel();
        let watchdog_expired = Arc::new(AtomicBool::new(false));
        let watchdog = std::thread::spawn({
            let watchdog_expired = Arc::clone(&watchdog_expired);
            move || {
                if watchdog_complete_recv
                    .recv_timeout(RELOAD_LOCK_WATCHDOG_TIMEOUT)
                    .is_err()
                {
                    watchdog_expired.store(true, Ordering::Release);
                }
                release_send
                    .send(())
                    .expect("watchdog releases repository mutex holder");
            }
        });

        let reload = tokio::spawn({
            let service = Arc::clone(&service);
            async move { service.reload().await }
        });
        tokio::time::timeout(RUNTIME_RESPONSIVENESS_TIMEOUT, reload_attempt_recv)
            .await
            .expect("reload must attempt the repository mutex on the blocking pool")
            .expect("reload repository lock attempt");
        assert!(
            !watchdog_expired.load(Ordering::Acquire),
            "a synchronous reload regression exhausted the release watchdog"
        );

        let (progressed_send, progressed_recv) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            progressed_send.send(()).expect("unrelated task progresses");
        });
        tokio::time::timeout(RUNTIME_RESPONSIVENESS_TIMEOUT, progressed_recv)
            .await
            .expect("unrelated task must progress while reload waits")
            .expect("unrelated task completion");
        assert!(
            !watchdog_expired.load(Ordering::Acquire),
            "a synchronous reload regression exhausted the release watchdog"
        );
        watchdog_complete_send
            .send(())
            .expect("complete reload responsiveness proof");
        watchdog.join().expect("join repository mutex watchdog");
        holder.await.expect("join holder");
        reload
            .await
            .expect("join reload")
            .expect("reload succeeds after repository mutex is released");
        assert!(
            service.snapshot().is_ok(),
            "reload republishes a safe snapshot"
        );
    }

    #[tokio::test]
    async fn lifecycle_preserves_coordinates_and_fresh_enrollment_retires_only_locally_unenrolled_state(
    ) {
        let service = HostGovernanceService::open_test();
        let active = activate_new_host(&service).await;
        assert_eq!(
            active.enrollment().expect("active enrollment").status(),
            EnrollmentStatus::Active
        );
        assert_eq!(
            active
                .enrollment()
                .expect("active enrollment")
                .revision()
                .get(),
            2
        );
        assert_eq!(active.owner_revision().expect("owner revision").get(), 1);

        service
            .apply_local_confirmation(confirmation(LocalGovernanceCommand::Rotate {
                expected: active,
            }))
            .await
            .expect("start rotation");
        let rotating = service.status().expect("rotating status");
        service
            .apply_local_confirmation(confirmation(LocalGovernanceCommand::LocallyUnenroll {
                expected: rotating,
            }))
            .await
            .expect("locally unenroll rotating host");
        let locally_unenrolled = service.status().expect("locally unenrolled status");
        assert_eq!(
            locally_unenrolled
                .enrollment()
                .expect("locally unenrolled enrollment")
                .status(),
            EnrollmentStatus::LocallyUnenrolled
        );
        assert_eq!(
            locally_unenrolled
                .enrollment()
                .expect("locally unenrolled enrollment")
                .revision()
                .get(),
            4
        );

        service
            .apply_local_confirmation(confirmation(LocalGovernanceCommand::Fresh {
                expected: locally_unenrolled,
                relay_id: relay(3),
                owner: principal(4),
            }))
            .await
            .expect("fresh confirmed enrollment");
        let fresh = service.status().expect("fresh pending status");
        assert_eq!(
            fresh.enrollment().expect("fresh enrollment").status(),
            EnrollmentStatus::PendingLocalCommit
        );
        assert_eq!(
            fresh
                .enrollment()
                .expect("fresh enrollment")
                .revision()
                .get(),
            5
        );
        assert_eq!(
            fresh.owner_revision().expect("fresh owner revision").get(),
            2
        );
        service
            .mutate_locked(|repository| Ok(repository.governance_state().clone()))
            .await
            .expect("read durable governance")
            .retired_enrollment()
            .expect("retired previous generation");
    }

    #[tokio::test]
    async fn disabled_host_requires_local_unenrollment_before_different_relay_fresh_enrollment() {
        let service = HostGovernanceService::open_test();
        let active = activate_new_host(&service).await;
        service
            .apply_local_confirmation(confirmation(LocalGovernanceCommand::Disable {
                expected: active,
            }))
            .await
            .expect("disable active host");
        let disabled = service.status().expect("disabled status");
        assert!(matches!(
            service
                .apply_local_confirmation(confirmation(LocalGovernanceCommand::Fresh {
                    expected: disabled.clone(),
                    relay_id: relay(9),
                    owner: principal(10),
                }))
                .await,
            Err(HostGovernanceError::Transition(_))
        ));
        service
            .apply_local_confirmation(confirmation(LocalGovernanceCommand::LocallyUnenroll {
                expected: disabled,
            }))
            .await
            .expect("explicit local unenrollment");
        let unenrolled = service.status().expect("locally unenrolled status");
        service
            .apply_local_confirmation(confirmation(LocalGovernanceCommand::Fresh {
                expected: unenrolled,
                relay_id: relay(9),
                owner: principal(10),
            }))
            .await
            .expect("fresh enrollment only after locally unenrolled state");
    }

    #[tokio::test]
    async fn transfer_retries_exactly_and_quarantines_conflicting_projection() {
        let service = HostGovernanceService::open_test();
        let active = activate_new_host(&service).await;
        let proposal = transfer(&active, 11, 12, principal(13));
        let outcome = service
            .apply_local_confirmation(confirmation(LocalGovernanceCommand::Transfer {
                proposal: proposal.clone(),
            }))
            .await
            .expect("transfer commits")
            .expect("transfer returns signed outcome");
        assert_eq!(
            service
                .retry_transfer(proposal.clone())
                .await
                .expect("exact retry returns durable outcome"),
            outcome
        );
        let changed = transfer(&active, 11, 14, principal(15));
        assert!(matches!(
            service.retry_transfer(changed).await,
            Err(HostGovernanceError::Transition(_))
        ));

        service
            .reconcile_projection(RelayProjection::Outcome(Box::new(outcome.clone())))
            .await
            .expect("exact projection remains healthy");
        service
            .reconcile_projection(RelayProjection::Missing)
            .await
            .expect("missing projection quarantines durably");
        let quarantined = service.status().expect("quarantined status");
        assert_eq!(
            quarantined
                .enrollment()
                .expect("quarantined enrollment")
                .status(),
            EnrollmentStatus::Quarantined
        );
        assert_eq!(
            quarantined.quarantine(),
            Some(QuarantineReason::ProjectionConflict)
        );
        let quarantined_revision = quarantined
            .enrollment()
            .expect("quarantined enrollment")
            .revision();
        service
            .reconcile_projection(RelayProjection::Missing)
            .await
            .expect("repeated mismatching projection is idempotent");
        assert_eq!(
            service.status().expect("unchanged quarantine"),
            quarantined,
            "repeated projection mismatch must not churn revision or provenance"
        );
        service
            .reconcile_projection(RelayProjection::Outcome(Box::new(outcome)))
            .await
            .expect("exact healthy projection restores the quarantined lifecycle");
        let recovered = service.status().expect("recovered status");
        assert_eq!(
            recovered
                .enrollment()
                .expect("recovered enrollment")
                .status(),
            EnrollmentStatus::Active
        );
        assert_eq!(
            recovered
                .enrollment()
                .expect("recovered enrollment")
                .revision(),
            quarantined_revision
                .checked_next()
                .expect("recovery revision advances")
        );
    }

    #[tokio::test]
    async fn projection_conflict_restart_recovers_only_its_exact_lifecycle_origin() {
        for origin in [
            EnrollmentStatus::PendingLocalCommit,
            EnrollmentStatus::Active,
            EnrollmentStatus::Disabled,
            EnrollmentStatus::Rotating,
            EnrollmentStatus::LocallyUnenrolled,
        ] {
            let root = tempfile::tempdir().expect("create durable service root");
            let service = open_service_at(root.path()).await;
            let before = establish_lifecycle(&service, origin).await;
            let before_revision = before
                .enrollment()
                .expect("enrolled lifecycle origin")
                .revision();
            service
                .quarantine(before.clone(), QuarantineReason::ProjectionConflict)
                .await
                .expect("persist projection-conflict quarantine");
            let quarantined = service.status().expect("quarantined status");
            assert_eq!(
                quarantined
                    .enrollment()
                    .expect("quarantined enrollment")
                    .revision(),
                before_revision
                    .checked_next()
                    .expect("quarantine revision advances")
            );
            service
                .quarantine(quarantined.clone(), QuarantineReason::ProjectionConflict)
                .await
                .expect("repeated matching quarantine is idempotent");
            assert_eq!(
                service.status().expect("unchanged quarantine"),
                quarantined,
                "repeated matching quarantine must not churn {origin:?}"
            );
            assert!(matches!(
                service
                    .quarantine(before, QuarantineReason::ProjectionConflict)
                    .await,
                Err(HostGovernanceError::Transition(
                    super::GovernanceTransitionError::StaleCoordinates
                ))
            ));

            drop(service);
            let reopened = open_service_at(root.path()).await;
            assert_eq!(
                reopened
                    .mutate_locked(|repository| {
                        Ok(repository.governance_state().quarantine_origin())
                    })
                    .await
                    .expect("read private durable provenance"),
                Some(origin),
                "restart retains exact private {origin:?} provenance"
            );
            reopened
                .reconcile_projection(RelayProjection::Missing)
                .await
                .expect("exact missing proof restores no-outcome lifecycle");
            let recovered = reopened.status().expect("recovered status");
            let recovered_enrollment = recovered.enrollment().expect("recovered enrollment");
            assert_eq!(recovered_enrollment.status(), origin);
            assert_eq!(recovered.quarantine(), None);
            assert_eq!(
                recovered_enrollment.revision(),
                before_revision
                    .checked_next()
                    .expect("quarantine revision advances")
                    .checked_next()
                    .expect("recovery revision advances")
            );
            assert_eq!(
                reopened
                    .mutate_locked(|repository| {
                        Ok(repository.governance_state().quarantine_origin())
                    })
                    .await
                    .expect("read cleared private provenance"),
                None
            );
        }
    }

    #[tokio::test]
    async fn signed_current_projection_survives_quarantine_restart_exact_recovery_and_retry() {
        let root = tempfile::tempdir().expect("create durable service root");
        let service = open_service_at(root.path()).await;
        let active = activate_new_host(&service).await;
        let proposal = transfer(&active, 121, 122, principal(123));
        let outcome = service
            .apply_local_confirmation(confirmation(LocalGovernanceCommand::Transfer {
                proposal: proposal.clone(),
            }))
            .await
            .expect("commit signed current transfer")
            .expect("return signed current outcome");
        let transferred = service.status().expect("post-transfer active status");
        let transferred_enrollment = transferred.enrollment().expect("active enrollment");
        service
            .reconcile_projection(RelayProjection::Missing)
            .await
            .expect("missing projection enters durable quarantine");
        let quarantined = service.status().expect("quarantined state");
        assert_eq!(
            quarantined.quarantine(),
            Some(QuarantineReason::ProjectionConflict)
        );
        assert_eq!(
            quarantined
                .enrollment()
                .expect("quarantined enrollment")
                .revision(),
            transferred_enrollment
                .revision()
                .checked_next()
                .expect("quarantine revision advances")
        );
        drop(service);

        let reopened = open_service_at(root.path()).await;
        reopened
            .reconcile_projection(RelayProjection::Outcome(Box::new(outcome.clone())))
            .await
            .expect("exact signed projection restores after restart");
        let recovered = reopened.status().expect("recovered status");
        assert_eq!(
            recovered
                .enrollment()
                .expect("recovered enrollment")
                .status(),
            EnrollmentStatus::Active
        );
        assert_eq!(
            recovered
                .enrollment()
                .expect("recovered enrollment")
                .revision(),
            quarantined
                .enrollment()
                .expect("quarantined enrollment")
                .revision()
                .checked_next()
                .expect("recovery revision advances")
        );
        assert_eq!(recovered.host_id(), transferred.host_id());
        assert_eq!(
            recovered
                .enrollment()
                .expect("recovered enrollment")
                .relay_id(),
            transferred_enrollment.relay_id()
        );
        assert_eq!(recovered.owner(), transferred.owner());
        assert_eq!(recovered.owner_revision(), transferred.owner_revision());
        assert_eq!(
            recovered.approval_key_reference(),
            transferred.approval_key_reference()
        );
        let retried = reopened
            .retry_transfer(proposal)
            .await
            .expect("exact original proposal retry remains durable after recovery");
        assert_eq!(retried, outcome);
        assert_eq!(
            serde_json::to_vec(&retried).expect("serialize retried outcome"),
            serde_json::to_vec(&outcome).expect("serialize original outcome"),
            "recovery preserves the exact signed outcome wire"
        );
    }

    #[tokio::test]
    async fn retained_generation_keeps_its_exact_transfer_retry_after_a_fresh_enrollment() {
        let service = HostGovernanceService::open_test();
        let active = activate_new_host(&service).await;
        let proposal = transfer(&active, 21, 22, principal(23));
        let outcome = service
            .apply_local_confirmation(confirmation(LocalGovernanceCommand::Transfer {
                proposal: proposal.clone(),
            }))
            .await
            .expect("transfer commits")
            .expect("signed transfer outcome");
        let after_transfer = service.status().expect("post-transfer state");
        service
            .apply_local_confirmation(confirmation(LocalGovernanceCommand::LocallyUnenroll {
                expected: after_transfer,
            }))
            .await
            .expect("locally unenroll transferred generation");
        let locally_unenrolled = service.status().expect("locally unenrolled state");
        service
            .apply_local_confirmation(confirmation(LocalGovernanceCommand::Fresh {
                expected: locally_unenrolled,
                relay_id: relay(24),
                owner: principal(25),
            }))
            .await
            .expect("fresh enrollment retires exactly one old generation");

        assert_eq!(
            service
                .retry_transfer(proposal)
                .await
                .expect("exact old-generation retry remains available"),
            outcome
        );
    }

    #[tokio::test]
    async fn relay_projection_uses_only_the_current_generation_outcome() {
        async fn fresh_active_after_first_transfer(
            service: &HostGovernanceService,
        ) -> (TransferProposal, SignedTransferOutcome) {
            let active = activate_new_host(service).await;
            let first = transfer(&active, 51, 52, principal(53));
            let outcome = service
                .apply_local_confirmation(confirmation(LocalGovernanceCommand::Transfer {
                    proposal: first.clone(),
                }))
                .await
                .expect("first transfer commits")
                .expect("first transfer outcome");
            let transferred = service.status().expect("transferred status");
            service
                .apply_local_confirmation(confirmation(LocalGovernanceCommand::LocallyUnenroll {
                    expected: transferred,
                }))
                .await
                .expect("locally unenroll first generation");
            let locally_unenrolled = service.status().expect("locally unenrolled status");
            service
                .apply_local_confirmation(confirmation(LocalGovernanceCommand::Fresh {
                    expected: locally_unenrolled,
                    relay_id: relay(54),
                    owner: principal(55),
                }))
                .await
                .expect("fresh enrollment");
            let pending = service.status().expect("fresh pending status");
            service
                .apply_local_confirmation(confirmation(LocalGovernanceCommand::Activate {
                    expected: pending,
                }))
                .await
                .expect("activate fresh generation");
            (first, outcome)
        }

        let missing_healthy = HostGovernanceService::open_test();
        let (_first, _first_outcome) = fresh_active_after_first_transfer(&missing_healthy).await;
        missing_healthy
            .reconcile_projection(RelayProjection::Missing)
            .await
            .expect("no current outcome means missing projection is healthy");
        assert_ne!(
            missing_healthy
                .status()
                .expect("healthy current status")
                .enrollment()
                .expect("current enrollment")
                .status(),
            EnrollmentStatus::Quarantined
        );

        let current = HostGovernanceService::open_test();
        let (_first, first_outcome) = fresh_active_after_first_transfer(&current).await;
        let active = current.status().expect("fresh active status");
        let second = transfer(&active, 56, 57, principal(58));
        let second_outcome = current
            .apply_local_confirmation(confirmation(LocalGovernanceCommand::Transfer {
                proposal: second,
            }))
            .await
            .expect("second transfer commits")
            .expect("second outcome");
        current
            .reconcile_projection(RelayProjection::Outcome(Box::new(second_outcome)))
            .await
            .expect("current exact outcome is healthy");
        current
            .reconcile_projection(RelayProjection::Outcome(Box::new(first_outcome)))
            .await
            .expect("retired outcome conflicts with current relay projection");
        assert_eq!(
            current
                .status()
                .expect("quarantined current status")
                .quarantine(),
            Some(QuarantineReason::ProjectionConflict)
        );
    }

    #[tokio::test]
    async fn exact_transfer_retry_survives_response_loss_restart_and_expiry() {
        let root = tempfile::tempdir().expect("create durable service root");
        let service = open_service_at(root.path()).await;
        let active = activate_new_host(&service).await;
        let proposal = transfer_with_expiry(&active, 61, 62, principal(63), "2026-01-02T00:00:00Z");
        let first_response = service
            .apply_local_confirmation_at(
                confirmation(LocalGovernanceCommand::Transfer {
                    proposal: proposal.clone(),
                }),
                at("2026-01-01T00:00:00Z"),
            )
            .await
            .expect("durable transfer whose response is lost")
            .expect("signed outcome before response loss");
        let first_wire = serde_json::to_string(&first_response)
            .expect("serialize first outcome through the canonical JSON wire");
        let first_for_verification = first_response.clone();
        service
            .mutate_locked(move |repository| {
                super::verify_outcome(repository, &first_for_verification)
            })
            .await
            .expect("strictly verify first durable outcome");
        drop(service);

        let reopened = open_service_at(root.path()).await;
        let before_expiry = reopened
            .apply_local_confirmation_at(
                confirmation(LocalGovernanceCommand::Transfer {
                    proposal: proposal.clone(),
                }),
                at("2026-01-01T12:00:00Z"),
            )
            .await
            .expect("exact durable retry before expiry")
            .expect("exact outcome before expiry");
        let at_expiry = reopened
            .apply_local_confirmation_at(
                confirmation(LocalGovernanceCommand::Transfer { proposal }),
                at("2026-01-02T00:00:00Z"),
            )
            .await
            .expect("exact durable retry takes precedence over expiry")
            .expect("exact outcome at expiry");

        for (stage, outcome) in [
            ("retry before expiry", &before_expiry),
            ("retry at expiry", &at_expiry),
        ] {
            let outcome = outcome.clone();
            reopened
                .mutate_locked(move |repository| super::verify_outcome(repository, &outcome))
                .await
                .unwrap_or_else(|_error| panic!("strictly verify {stage} outcome"));
        }

        assert_eq!(first_response, before_expiry);
        assert_eq!(first_response, at_expiry);
        assert_eq!(
            first_wire,
            serde_json::to_string(&before_expiry)
                .expect("serialize retry-before-expiry through the canonical JSON wire")
        );
        assert_eq!(
            first_wire,
            serde_json::to_string(&at_expiry)
                .expect("serialize retry-at-expiry through the canonical JSON wire")
        );
    }

    #[tokio::test]
    async fn precommit_and_postrename_transfer_failures_preserve_the_required_publication_boundary()
    {
        let root = tempfile::tempdir().expect("create durable service root");
        let service = open_service_at(root.path()).await;
        let active = activate_new_host(&service).await;
        let precommit = transfer(&active, 71, 72, principal(73));
        service
            .inject_governance_rename_failure()
            .await
            .expect("inject precommit rename failure");
        assert!(matches!(
            service
                .apply_local_confirmation(confirmation(LocalGovernanceCommand::Transfer {
                    proposal: precommit,
                }))
                .await,
            Err(HostGovernanceError::Persistence(_))
        ));
        assert_eq!(
            service.status().expect("old snapshot remains published"),
            active
        );

        let postrename = transfer(&active, 74, 75, principal(76));
        service
            .inject_governance_directory_sync_failure()
            .await
            .expect("inject postrename directory sync failure");
        assert!(matches!(
            service
                .apply_local_confirmation(confirmation(LocalGovernanceCommand::Transfer {
                    proposal: postrename.clone(),
                }))
                .await,
            Err(HostGovernanceError::Persistence(
                HostStateRepositoryError::DurabilityUncertain {
                    record: "governance.json"
                }
            ))
        ));
        assert!(matches!(
            service.snapshot(),
            Err(HostGovernanceError::ReloadRequired)
        ));
        assert_eq!(
            service.diagnose().await,
            HostGovernanceDiagnostic::ReloadRequired,
            "doctor diagnostics must not treat uncertain durability as available"
        );
        drop(service);

        let reopened = open_service_at(root.path()).await;
        let status = reopened
            .status()
            .expect("reopen after uncertain setup write");
        assert!(
            status
                .owner_revision()
                .is_some_and(|revision| revision.get() == 1 || revision.get() == 2),
            "reopen classifies exactly one old-or-new durable state"
        );
        let retry = reopened.retry_transfer(postrename).await;
        if status.owner_revision().expect("owner revision").get() == 2 {
            retry.expect("new durable state supplies its exact retry");
        } else {
            assert!(matches!(retry, Err(HostGovernanceError::Transition(_))));
        }
    }

    #[tokio::test]
    async fn simultaneous_conflicting_transfers_have_one_durable_winner() {
        use tokio::sync::Barrier;

        let service = HostGovernanceService::open_test();
        let active = activate_new_host(&service).await;
        let first = transfer(&active, 31, 32, principal(33));
        let second = transfer(&active, 34, 35, principal(36));
        let barrier = Arc::new(Barrier::new(2));
        let first_service = service.clone();
        let first_barrier = Arc::clone(&barrier);
        let first_task = tokio::spawn(async move {
            first_barrier.wait().await;
            first_service
                .apply_local_confirmation(confirmation(LocalGovernanceCommand::Transfer {
                    proposal: first,
                }))
                .await
        });
        let second_service = service.clone();
        let second_task = tokio::spawn(async move {
            barrier.wait().await;
            second_service
                .apply_local_confirmation(confirmation(LocalGovernanceCommand::Transfer {
                    proposal: second,
                }))
                .await
        });
        let first = first_task.await.expect("first transfer task joins");
        let second = second_task.await.expect("second transfer task joins");
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        assert!(matches!(
            first.err().or(second.err()),
            Some(HostGovernanceError::Transition(_))
        ));
        assert_eq!(
            service
                .status()
                .expect("winner remains durable")
                .owner_revision()
                .expect("owner revision")
                .get(),
            2
        );
    }

    #[tokio::test]
    async fn simultaneous_identical_confirmed_transfers_converge_on_one_signed_outcome() {
        use tokio::sync::Barrier;

        let service = HostGovernanceService::open_test();
        let active = activate_new_host(&service).await;
        let proposal = transfer(&active, 91, 92, principal(93));
        let barrier = Arc::new(Barrier::new(2));
        let first_service = service.clone();
        let first_barrier = Arc::clone(&barrier);
        let first_proposal = proposal.clone();
        let first = tokio::spawn(async move {
            first_barrier.wait().await;
            first_service
                .apply_local_confirmation(confirmation(LocalGovernanceCommand::Transfer {
                    proposal: first_proposal,
                }))
                .await
        });
        let second_service = service.clone();
        let second = tokio::spawn(async move {
            barrier.wait().await;
            second_service
                .apply_local_confirmation(confirmation(LocalGovernanceCommand::Transfer {
                    proposal,
                }))
                .await
        });
        let first = first
            .await
            .expect("first identical transfer joins")
            .expect("first identical transfer succeeds")
            .expect("first identical transfer returns outcome");
        let second = second
            .await
            .expect("second identical transfer joins")
            .expect("second identical transfer succeeds")
            .expect("second identical transfer returns outcome");
        assert_eq!(first, second);
        assert_eq!(
            service
                .status()
                .expect("single durable transfer state")
                .owner_revision()
                .expect("owner revision")
                .get(),
            2
        );
    }

    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "One table intentionally renders every replay-sensitive transfer coordinate visible."
    )]
    async fn changed_transfer_coordinates_and_replay_material_are_rejected_without_mutation() {
        let service = HostGovernanceService::open_test();
        let active = activate_new_host(&service).await;
        let proposal = transfer(&active, 101, 102, principal(103));
        service
            .apply_local_confirmation(confirmation(LocalGovernanceCommand::Transfer {
                proposal: proposal.clone(),
            }))
            .await
            .expect("baseline transfer commits");
        let durable = service.snapshot().expect("durable baseline snapshot");
        let enrollment = active.enrollment().expect("original enrollment");
        let owner = active.owner().expect("original owner");
        let revision = active.owner_revision().expect("original revision");
        let host = active.host_id().clone();
        let target = proposal.target().clone();
        let cases = [
            (
                "proposal id",
                transfer_with_coordinates(
                    enrollment.relay_id().clone(),
                    host.clone(),
                    revision,
                    owner.clone(),
                    target.clone(),
                    104,
                    102,
                    "2030-01-01T00:00:00Z",
                ),
            ),
            (
                "nonce",
                transfer_with_coordinates(
                    enrollment.relay_id().clone(),
                    host.clone(),
                    revision,
                    owner.clone(),
                    target.clone(),
                    101,
                    105,
                    "2030-01-01T00:00:00Z",
                ),
            ),
            (
                "host",
                transfer_with_coordinates(
                    enrollment.relay_id().clone(),
                    HostId::parse(&identifier("host_", 106)).expect("different host"),
                    revision,
                    owner.clone(),
                    target.clone(),
                    101,
                    102,
                    "2030-01-01T00:00:00Z",
                ),
            ),
            (
                "relay",
                transfer_with_coordinates(
                    relay(107),
                    host.clone(),
                    revision,
                    owner.clone(),
                    target.clone(),
                    101,
                    102,
                    "2030-01-01T00:00:00Z",
                ),
            ),
            (
                "current owner kind and id",
                transfer_with_coordinates(
                    enrollment.relay_id().clone(),
                    host.clone(),
                    revision,
                    team(108),
                    target.clone(),
                    101,
                    102,
                    "2030-01-01T00:00:00Z",
                ),
            ),
            (
                "target kind and id",
                transfer_with_coordinates(
                    enrollment.relay_id().clone(),
                    host.clone(),
                    revision,
                    owner.clone(),
                    team(109),
                    101,
                    102,
                    "2030-01-01T00:00:00Z",
                ),
            ),
            (
                "owner revision",
                transfer_with_coordinates(
                    enrollment.relay_id().clone(),
                    host.clone(),
                    revision.checked_next().expect("next revision"),
                    owner.clone(),
                    target.clone(),
                    101,
                    102,
                    "2030-01-01T00:00:00Z",
                ),
            ),
            (
                "expiry",
                transfer_with_coordinates(
                    enrollment.relay_id().clone(),
                    host,
                    revision,
                    owner.clone(),
                    target,
                    101,
                    102,
                    "2030-01-01T00:00:01Z",
                ),
            ),
        ];
        for (coordinate, changed) in cases {
            assert!(matches!(
                service.retry_transfer(changed).await,
                Err(HostGovernanceError::Transition(_))
            ));
            assert_eq!(
                service.snapshot().expect("rejection preserves snapshot"),
                durable,
                "{coordinate} rejection must not mutate durable governance"
            );
        }
    }

    #[tokio::test]
    async fn signed_projection_tampering_quarantines_without_accepting_candidate_changes() {
        async fn assert_tamper(pointer: &str, replacement: serde_json::Value) {
            let service = HostGovernanceService::open_test();
            let active = activate_new_host(&service).await;
            let outcome = service
                .apply_local_confirmation(confirmation(LocalGovernanceCommand::Transfer {
                    proposal: transfer(&active, 111, 112, principal(113)),
                }))
                .await
                .expect("baseline transfer commits")
                .expect("baseline outcome");
            let mut wire = serde_json::to_value(outcome).expect("serialize signed outcome");
            *wire.pointer_mut(pointer).expect("candidate field exists") = replacement;
            let tampered = serde_json::from_value(wire).expect("well-formed tampered outcome");
            service
                .reconcile_projection(RelayProjection::Outcome(Box::new(tampered)))
                .await
                .expect("tampered projection is durably quarantined");
            assert_eq!(
                service.status().expect("quarantined status").quarantine(),
                Some(QuarantineReason::ProjectionConflict),
                "{pointer} must not be accepted as a current relay projection"
            );
        }

        for (pointer, replacement) in [
            (
                "/signature",
                serde_json::json!(format!(
                    "sig_{}",
                    BASE64_URL_SAFE_NO_PAD.encode([114_u8; 64])
                )),
            ),
            (
                "/candidate/proposal/host_id",
                serde_json::json!(identifier("host_", 115)),
            ),
            (
                "/candidate/proposal/relay_id",
                serde_json::json!(identifier("relay_", 116)),
            ),
            (
                "/candidate/proposal/current_owner",
                serde_json::json!({"kind": "team", "id": identifier("team_", 117)}),
            ),
            (
                "/candidate/proposal/target",
                serde_json::json!({"kind": "team", "id": identifier("team_", 118)}),
            ),
            (
                "/candidate/proposal/nonce",
                serde_json::json!(identifier("nonce_", 119)),
            ),
            (
                "/candidate/proposal/expiry",
                serde_json::json!("2030-01-01T00:00:01Z"),
            ),
        ] {
            assert_tamper(pointer, replacement).await;
        }
    }

    #[tokio::test]
    async fn quarantine_requires_local_unenrollment_for_fresh_enrollment_and_exact_proof_cannot_clear_other_conflicts(
    ) {
        for reason in [
            QuarantineReason::ProjectionConflict,
            QuarantineReason::HostIdentityClone,
            QuarantineReason::EnrollmentConflict,
        ] {
            let service = HostGovernanceService::open_test();
            let active = activate_new_host(&service).await;
            let expected_owner = active.owner().expect("active owner").clone();
            let expected_owner_revision = active.owner_revision().expect("active owner revision");
            service
                .quarantine(active, reason)
                .await
                .expect("conflict quarantines durably");
            let quarantined = service.status().expect("quarantined state");
            assert_eq!(quarantined.quarantine(), Some(reason));
            if reason != QuarantineReason::ProjectionConflict {
                service
                    .reconcile_projection(RelayProjection::Missing)
                    .await
                    .expect("exact missing proof is observed");
                assert_eq!(
                    service.status().expect("non-projection quarantine remains"),
                    quarantined,
                    "exact projection evidence must not clear {reason:?}"
                );
            }
            assert!(matches!(
                service
                    .apply_local_confirmation(confirmation(LocalGovernanceCommand::Fresh {
                        expected: quarantined.clone(),
                        relay_id: relay(81),
                        owner: principal(82),
                    }))
                    .await,
                Err(HostGovernanceError::Transition(
                    super::GovernanceTransitionError::InvalidLifecycle
                ))
            ));
            service
                .apply_local_confirmation(confirmation(LocalGovernanceCommand::LocallyUnenroll {
                    expected: quarantined,
                }))
                .await
                .expect("explicit local unenrollment clears quarantine");
            let locally_unenrolled = service.status().expect("locally unenrolled state");
            assert_eq!(locally_unenrolled.quarantine(), None);
            assert_eq!(locally_unenrolled.owner(), Some(&expected_owner));
            assert_eq!(
                locally_unenrolled.owner_revision(),
                Some(expected_owner_revision)
            );
            service
                .apply_local_confirmation(confirmation(LocalGovernanceCommand::Fresh {
                    expected: locally_unenrolled,
                    relay_id: relay(81),
                    owner: principal(82),
                }))
                .await
                .expect("fresh enrollment is permitted only after local unenrollment");
        }
    }

    #[tokio::test]
    async fn stale_commands_expired_proposals_and_revision_overflow_fail_closed() {
        let service = HostGovernanceService::open_test();
        let active = activate_new_host(&service).await;
        service
            .apply_local_confirmation(confirmation(LocalGovernanceCommand::Rotate {
                expected: active.clone(),
            }))
            .await
            .expect("rotation starts");
        assert!(matches!(
            service
                .apply_local_confirmation(confirmation(LocalGovernanceCommand::Disable {
                    expected: active,
                }))
                .await,
            Err(HostGovernanceError::Transition(
                super::GovernanceTransitionError::StaleCoordinates
            ))
        ));

        let service = HostGovernanceService::open_test();
        let active = activate_new_host(&service).await;
        let expired = TransferProposal::new(
            protocol::TransferCoordinates::new(
                active
                    .enrollment()
                    .expect("active enrollment")
                    .relay_id()
                    .clone(),
                active.host_id().clone(),
                active.owner_revision().expect("owner revision"),
                active.owner().expect("active owner").clone(),
                principal(41),
            ),
            protocol::ProposalId::parse(&identifier("proposal_", 42)).expect("proposal id"),
            protocol::ProposalNonce::parse(&identifier("nonce_", 43)).expect("nonce"),
            protocol::ProposalExpiry::parse("2000-01-01T00:00:00Z").expect("expired time"),
        );
        assert!(matches!(
            service
                .apply_local_confirmation(confirmation(LocalGovernanceCommand::Transfer {
                    proposal: expired,
                }))
                .await,
            Err(HostGovernanceError::Transition(
                super::GovernanceTransitionError::ProposalExpired
            ))
        ));

        service
            .mutate_locked(|repository| {
                let current = repository.governance_state().clone();
                let status = HostGovernanceStatus::new(
                    current.status().host_id().clone(),
                    current.status().enrollment().cloned(),
                    current.status().owner().cloned(),
                    Some(protocol::OwnerRevision::new(u64::MAX).expect("nonzero maximum")),
                    None,
                    current.status().approval_key_reference().clone(),
                )
                .map_err(|_error| super::GovernanceTransitionError::InvalidLifecycle)?;
                let replacement = GovernanceState::new(status, None, None)
                    .map_err(HostGovernanceError::Persistence)?;
                repository
                    .replace_governance(&current, replacement)
                    .map_err(HostGovernanceError::Persistence)
            })
            .await
            .expect("seed checked owner-revision boundary");
        let maximum = service.status().expect("maximum revision state");
        assert!(matches!(
            service
                .apply_local_confirmation(confirmation(LocalGovernanceCommand::Transfer {
                    proposal: transfer(&maximum, 44, 45, principal(46)),
                }))
                .await,
            Err(HostGovernanceError::Transition(
                super::GovernanceTransitionError::RevisionOverflow
            ))
        ));
    }
}
