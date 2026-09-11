//! Performs local bootstrap and recovery-quarantine lifecycle transitions.
//!
//! This module is callable only by the protected local runtime. It keeps the
//! independent witness ahead of a restored database and derives review evidence
//! from durable authority state instead of caller-provided approval flags.

// Rust guideline compliant 2026-09-08

mod local;

pub(crate) use local::canonical_initial_provision_request;

use std::sync::Arc;

use sha2::{Digest, Sha256};
use sqlx::{Postgres, Row, Transaction};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    recovery::{DenyIncident, IncidentReview, RecoveryError, WitnessRecord, WitnessStore},
    store::{lease::LeaseGuard, Store},
};

const MANIFEST_DOMAIN: &[u8] = b"pohunek.relay.recovery-manifest.v1\0";
const MAX_IDENTITY_BYTES: usize = 2_048;
/// Caps a reviewed snapshot so recovery cannot exhaust memory on authority data.
const MAX_MANIFEST_ROWS: usize = 4_096;
/// Bounds database-to-process manifest material before text values are decoded.
const MAX_MANIFEST_BYTES: i64 = 1_048_576;
const MANIFEST_VERSION: u16 = 2;

/// Identifies the only local infrastructure administrator created at bootstrap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapRequest {
    pub relay_id: String,
    pub issuer: String,
    pub subject: String,
}

/// Binds the one local initial-team operation to durable owner and delivery coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitialProvisionRequest {
    /// Relay coordinate that must match the stopped local authority.
    pub relay_id: String,
    /// Configured issuer of the bootstrap identity.
    pub issuer: String,
    /// Protected bootstrap identity subject selected as the explicit team owner.
    pub subject: String,
    /// Bounded initial team display name.
    pub team_name: String,
    /// Bounded service-account display name.
    pub service_account_name: String,
    /// Explicit bounded service credential expiry.
    pub expires_at: OffsetDateTime,
    /// Hash of the owner-private credential artifact.
    pub artifact_digest: [u8; 32],
    /// HMAC verifier of the sole delivered credential secret.
    pub credential_secret_digest: [u8; 32],
    /// Active digest key coordinate.
    pub digest_key_id: String,
    /// Preallocated durable team coordinate.
    pub team_id: Uuid,
    /// Preallocated explicit owner membership coordinate.
    pub owner_membership_id: Uuid,
    /// Preallocated service principal coordinate.
    pub service_principal_id: Uuid,
    /// Preallocated service membership coordinate.
    pub service_membership_id: Uuid,
    /// Preallocated credential coordinate.
    pub credential_id: Uuid,
    /// Preallocated audit coordinate.
    pub audit_id: Uuid,
}

/// Holds a digest of a complete, locally derived authority review manifest.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ReviewedManifest {
    version: u16,
    rows: Vec<ManifestRow>,
    incidents: Vec<DenyIncident>,
    incident_digest: [u8; 32],
    digest: [u8; 32],
}

/// Contains one bounded, operator-reviewable authority coordinate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct ManifestRow {
    kind: ManifestKind,
    fields: Vec<ManifestField>,
}

/// Names an authority relation represented in a recovery review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestKind {
    RelayIdentity,
    Principal,
    OidcIdentity,
    Team,
    Membership,
    CustomRole,
    CustomRolePermission,
    MembershipCustomRole,
    Group,
    GroupMember,
    Grant,
    HostOwnership,
    Revocation,
    AuditEvent,
    Cancellation,
    RestoreReview,
    AccountLink,
    BrowserLogin,
    DeviceLogin,
    BrowserSession,
    Credential,
    EvidenceChallenge,
    AdmissionRule,
    EvidenceResult,
    HistorySummary,
    ServiceAccount,
}

/// Names a safe, canonical field in one recovery review row.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct ManifestField {
    name: &'static str,
    value: Option<String>,
}

impl ReviewedManifest {
    /// Returns the semantic-review format version.
    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }

    /// Returns the fixed-size digest used by witness and database review records.
    #[must_use]
    pub const fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    /// Returns the versioned semantic rows an operator reviews before reopen.
    #[must_use]
    pub fn rows(&self) -> &[ManifestRow] {
        &self.rows
    }

    /// Returns witnessed denial scopes which must be reconciled before reopen.
    #[must_use]
    pub fn incidents(&self) -> &[DenyIncident] {
        &self.incidents
    }

    /// Returns the witnessed incident-set digest bound into this review.
    #[must_use]
    pub const fn incident_digest(&self) -> &[u8; 32] {
        &self.incident_digest
    }

    fn from_rows(
        rows: Vec<ManifestRow>,
        incidents: &IncidentReview,
    ) -> Result<Self, LifecycleError> {
        if rows.len() > MAX_MANIFEST_ROWS
            || !rows.windows(2).all(|pair| pair[0].kind <= pair[1].kind)
        {
            return Err(LifecycleError::Durable);
        }
        let mut hash = Sha256::new();
        hash.update(MANIFEST_DOMAIN);
        hash.update(MANIFEST_VERSION.to_be_bytes());
        hash.update(incidents.digest());
        for incident in incidents.incidents() {
            hash.update(serde_json::to_vec(incident).map_err(|_error| LifecycleError::Durable)?);
            hash.update([0]);
        }
        for row in &rows {
            hash.update([row.kind as u8]);
            for field in &row.fields {
                hash.update(field.name.as_bytes());
                hash.update([0]);
                if let Some(value) = &field.value {
                    hash.update([1]);
                    hash.update(value.as_bytes());
                } else {
                    hash.update([0]);
                }
                hash.update([0]);
            }
        }
        Ok(Self {
            version: MANIFEST_VERSION,
            rows,
            incidents: incidents.incidents().to_vec(),
            incident_digest: *incidents.digest(),
            digest: hash.finalize().into(),
        })
    }
}

impl ManifestRow {
    /// Returns the relation represented by this recovery row.
    #[must_use]
    pub const fn kind(&self) -> ManifestKind {
        self.kind
    }

    /// Returns the canonical safe fields for this recovery row.
    #[must_use]
    pub fn fields(&self) -> &[ManifestField] {
        &self.fields
    }
}

impl ManifestField {
    /// Returns the fixed canonical field name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Returns the safe value, absent for SQL NULL.
    #[must_use]
    pub fn value(&self) -> Option<&str> {
        self.value.as_deref()
    }
}

/// Performs local-only bootstrap and restore transitions.
#[derive(Debug, Clone)]
pub struct Lifecycle {
    store: Store,
    witness: Arc<WitnessStore>,
}

/// Reports a fail-closed lifecycle transition failure.
#[derive(Debug, Error)]
pub enum LifecycleError {
    #[error("relay lifecycle witness is unavailable")]
    Witness(#[from] RecoveryError),
    #[error("relay lifecycle durable state is unavailable")]
    Durable,
    #[error("relay lifecycle state is not eligible for this transition")]
    InvalidState,
    #[error("relay lifecycle review manifest does not match durable authority")]
    ManifestMismatch,
}

impl Lifecycle {
    /// Creates a lifecycle service from process-owned durable dependencies.
    #[must_use]
    pub fn new(store: Store, witness: Arc<WitnessStore>) -> Self {
        Self { store, witness }
    }

    /// Derives a complete semantic authority manifest under a serializable snapshot.
    pub async fn snapshot_authority_manifest(&self) -> Result<ReviewedManifest, LifecycleError> {
        let mut tx = self.begin().await?;
        let checkpoint = self.witness.latest()?.ok_or(LifecycleError::InvalidState)?;
        let incidents = self.witness.incident_review(&checkpoint)?;
        let manifest = semantic_manifest(&mut tx, incidents, false).await?;
        tx.commit()
            .await
            .map_err(|_error| LifecycleError::Durable)?;
        Ok(manifest)
    }

    /// Advances a reviewed restore checkpoint while excluding a live serving lease.
    pub async fn advance_restore_local(
        &self,
        reviewed_digest: &[u8; 32],
    ) -> Result<WitnessRecord, LifecycleError> {
        let current = self.witness.latest()?.ok_or(LifecycleError::InvalidState)?;
        let mut tx = self.begin().await?;
        self.store
            .lock_relay_identity_in_transaction(&mut tx, &current.relay_id)
            .await
            .map_err(|_error| LifecycleError::InvalidState)?;
        self.require_no_live_lease(&mut tx, &current.relay_id)
            .await?;
        if current.active_run && current.recovery_pending_review {
            if current.manifest_digest != hex::encode(reviewed_digest) {
                return Err(LifecycleError::ManifestMismatch);
            }
            tx.commit()
                .await
                .map_err(|_error| LifecycleError::Durable)?;
            return Ok(current);
        }
        let incidents = self.witness.incident_review(&current)?;
        let manifest = semantic_manifest(&mut tx, incidents, false).await?;
        if manifest.digest() != reviewed_digest {
            return Err(LifecycleError::ManifestMismatch);
        }
        // Hold the identity row until the independent checkpoint is durable;
        // lease acquisition cannot race the operator's no-live-lease check.
        let advanced = self.advance_restore_witness(&current, &manifest)?;
        tx.commit()
            .await
            .map_err(|_error| LifecycleError::Durable)?;
        Ok(advanced)
    }

    /// Validates an ordinary clean restart without changing credential generation.
    ///
    /// Browser and device transactions cannot survive process-local PKCE or
    /// device-code loss, and external evidence is deliberately made unknown.
    /// Existing credentials and browser sessions remain current at this same
    /// recovery generation.
    ///
    /// # Errors
    /// Returns an error when the witness coordinate does not exactly match a
    /// normal relay identity.
    pub async fn validate_normal_start(
        &self,
        witness: &WitnessRecord,
    ) -> Result<i64, LifecycleError> {
        if witness.active_run {
            return Err(LifecycleError::InvalidState);
        }
        if self.witness.latest()?.as_ref() != Some(witness) {
            return Err(LifecycleError::InvalidState);
        }
        let mut tx = self.begin().await?;
        let current = sqlx::query_scalar::<_, i64>(
            "SELECT recovery_generation FROM relay_identity WHERE relay_id=$1 AND state='normal' AND recovery_generation=$2 FOR UPDATE",
        )
        .bind(&witness.relay_id)
        .bind(witness.recovery_generation)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_error| LifecycleError::Durable)?
        .ok_or(LifecycleError::InvalidState)?;
        tx.commit()
            .await
            .map_err(|_error| LifecycleError::Durable)?;
        Ok(current)
    }

    /// Invalidates process-local transactions after this process owns the lease.
    ///
    /// # Errors
    /// Returns an error when the lease is stale or durable invalidation fails.
    pub async fn finalize_normal_start(&self, lease: &LeaseGuard) -> Result<(), LifecycleError> {
        let mut tx = self.begin().await?;
        self.store
            .verify_lease_in_transaction(&mut tx, lease)
            .await
            .map_err(|_error| LifecycleError::InvalidState)?;
        for statement in [
            "UPDATE browser_logins SET consumed_at=clock_timestamp(),outcome='cancelled' WHERE consumed_at IS NULL",
            "UPDATE device_logins SET consumed_at=clock_timestamp(),outcome='cancelled',poll_lease_until=NULL WHERE consumed_at IS NULL",
            "UPDATE evidence_challenges SET consumed_at=clock_timestamp() WHERE consumed_at IS NULL",
        ] {
            sqlx::query(statement)
                .execute(&mut *tx)
                .await
                .map_err(|_error| LifecycleError::Durable)?;
        }
        // This second exact check is the commit deadline: a successor cannot
        // acquire the locked lease before this transaction commits.
        self.store
            .verify_lease_in_transaction(&mut tx, lease)
            .await
            .map_err(|_error| LifecycleError::InvalidState)?;
        tx.commit().await.map_err(|_error| LifecycleError::Durable)
    }

    /// Advances the independent witness before an operator restores `PostgreSQL`.
    pub fn advance_restore_witness(
        &self,
        current: &WitnessRecord,
        manifest: &ReviewedManifest,
    ) -> Result<WitnessRecord, LifecycleError> {
        let incidents = self.witness.incident_review(current)?;
        if manifest.incidents != incidents.incidents()
            || manifest.incident_digest != *incidents.digest()
        {
            return Err(LifecycleError::ManifestMismatch);
        }
        self.witness
            .advance_restore(
                current,
                hex::encode(manifest.digest()),
                hex::encode(incidents.digest()),
            )
            .map_err(Into::into)
    }

    /// Quarantines restored `PostgreSQL` state against an already advanced witness.
    pub async fn quarantine_restored(
        &self,
        witness: &WitnessRecord,
        manifest: &ReviewedManifest,
    ) -> Result<(), LifecycleError> {
        if self.witness.latest()?.as_ref() != Some(witness) || !witness.active_run {
            return Err(LifecycleError::InvalidState);
        }
        if witness.manifest_digest != hex::encode(manifest.digest()) {
            return Err(LifecycleError::ManifestMismatch);
        }
        if witness.incident_digest != hex::encode(manifest.incident_digest()) {
            return Err(LifecycleError::ManifestMismatch);
        }
        self.quarantine_checkpoint(witness, manifest.digest(), manifest.incidents())
            .await
    }

    /// Quarantines a restored database using the independently signed restore checkpoint.
    ///
    /// This is the command-line recovery entry point after the pre-restore process
    /// has exited. The checkpoint retains the reviewed digest and denial evidence;
    /// no caller-supplied manifest can replace them.
    pub async fn quarantine_pending(&self) -> Result<(), LifecycleError> {
        let witness = self.witness.latest()?.ok_or(LifecycleError::InvalidState)?;
        if !witness.active_run || !witness.recovery_pending_review {
            return Err(LifecycleError::InvalidState);
        }
        let incidents = self.witness.incident_review_at(&witness)?;
        if witness.incident_digest != hex::encode(incidents.digest()) {
            return Err(LifecycleError::ManifestMismatch);
        }
        let mut digest = [0_u8; 32];
        hex::decode_to_slice(&witness.manifest_digest, &mut digest)
            .map_err(|_error| LifecycleError::ManifestMismatch)?;
        self.quarantine_checkpoint(&witness, &digest, incidents.incidents())
            .await
    }

    async fn quarantine_checkpoint(
        &self,
        witness: &WitnessRecord,
        manifest_digest: &[u8; 32],
        incidents: &[DenyIncident],
    ) -> Result<(), LifecycleError> {
        if self.witness.latest()?.as_ref() != Some(witness)
            || !witness.active_run
            || !witness.recovery_pending_review
        {
            return Err(LifecycleError::InvalidState);
        }
        let mut tx = self.begin().await?;
        self.store
            .lock_relay_identity_in_transaction(&mut tx, &witness.relay_id)
            .await
            .map_err(|_error| LifecycleError::InvalidState)?;
        self.require_no_live_lease(&mut tx, &witness.relay_id)
            .await?;
        let already_quarantined: bool = sqlx::query_scalar("SELECT state='recovery_quarantine' AND recovery_generation=$2 FROM relay_identity WHERE relay_id=$1")
            .bind(&witness.relay_id).bind(witness.recovery_generation)
            .fetch_one(&mut *tx).await.map_err(|_error| LifecycleError::Durable)?;
        if already_quarantined {
            quarantine_review_matches(&mut tx, witness).await?;
            tx.commit()
                .await
                .map_err(|_error| LifecycleError::Durable)?;
            return Ok(());
        }
        let row = sqlx::query("UPDATE relay_identity SET recovery_generation=$2,state='recovery_quarantine',revision=revision+1,updated_at=clock_timestamp() WHERE relay_id=$1 AND recovery_generation < $2 RETURNING relay_id")
            .bind(&witness.relay_id).bind(witness.recovery_generation).fetch_optional(&mut *tx).await.map_err(|_error| LifecycleError::Durable)?;
        if row.is_none() {
            return Err(LifecycleError::InvalidState);
        }
        invalidate_old_generation(&mut tx, &witness.relay_id, witness.recovery_generation).await?;
        if matches!(incidents, [DenyIncident::Global { relay_id }] if relay_id == &witness.relay_id)
        {
            record_global_overflow_reconciliation(
                &mut tx,
                &witness.relay_id,
                witness.recovery_generation,
            )
            .await?;
        }
        let audit_id = audit(
            &mut tx,
            "lifecycle.restore.quarantine",
            "revoke",
            witness.recovery_generation,
            "recovery",
            "committed",
        )
        .await?;
        sqlx::query("INSERT INTO restore_reviews (review_id,relay_id,witness_generation,manifest_digest,action,audit_id) VALUES ($1,$2,$3,$4,'quarantine',$5)")
            .bind(Uuid::now_v7()).bind(&witness.relay_id).bind(witness.recovery_generation).bind(manifest_digest.as_slice()).bind(audit_id)
            .execute(&mut *tx).await.map_err(|_error| LifecycleError::Durable)?;
        tx.commit().await.map_err(|_error| LifecycleError::Durable)
    }

    /// Reopens quarantined authority after an exact local semantic review.
    pub async fn reopen_local(
        &self,
        witness: &WitnessRecord,
        reviewed: &ReviewedManifest,
    ) -> Result<WitnessRecord, LifecycleError> {
        if !witness.active_run || !witness.recovery_pending_review {
            return Err(LifecycleError::InvalidState);
        }
        let review_digest = hex::encode(reviewed.digest());
        let review_published = self
            .witness
            .recovery_review_is_published(witness, &review_digest)?;
        if self.witness.latest()?.as_ref() != Some(witness) && !review_published {
            return Err(LifecycleError::InvalidState);
        }
        let mut tx = self.begin().await?;
        self.store
            .lock_relay_identity_in_transaction(&mut tx, &witness.relay_id)
            .await
            .map_err(|_error| LifecycleError::InvalidState)?;
        self.require_no_live_lease(&mut tx, &witness.relay_id)
            .await?;
        quarantine_review_matches(&mut tx, witness).await?;
        let incidents = if review_published {
            IncidentReview::from_scopes(reviewed.incidents()).map_err(LifecycleError::Witness)?
        } else {
            self.witness.incident_review(witness)?
        };
        let state: String =
            sqlx::query_scalar("SELECT state FROM relay_identity WHERE relay_id=$1")
                .bind(&witness.relay_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(|_error| LifecycleError::Durable)?;
        let already_committed = reopen_review_matches(&mut tx, witness, reviewed).await?;
        let computed = semantic_manifest(&mut tx, incidents, state == "normal").await?;
        if computed != *reviewed
            || witness.incident_digest != hex::encode(reviewed.incident_digest())
            || unreconciled_incidents(
                &mut tx,
                &witness.relay_id,
                witness.recovery_generation,
                reviewed.incidents(),
            )
            .await?
            || unreconciled_host_state(&mut tx).await?
        {
            return Err(LifecycleError::ManifestMismatch);
        }
        if state == "normal" {
            if !already_committed {
                return Err(LifecycleError::InvalidState);
            }
            tx.commit()
                .await
                .map_err(|_error| LifecycleError::Durable)?;
            return self
                .witness
                .complete_recovery_review(witness, &review_digest)
                .map_err(Into::into);
        }
        if state != "recovery_quarantine" || already_committed {
            return Err(LifecycleError::InvalidState);
        }
        let updated = sqlx::query("UPDATE relay_identity SET state='normal',revision=revision+1,updated_at=clock_timestamp() WHERE relay_id=$1 AND recovery_generation=$2 AND state='recovery_quarantine'")
            .bind(&witness.relay_id).bind(witness.recovery_generation).execute(&mut *tx).await.map_err(|_error| LifecycleError::Durable)?;
        if updated.rows_affected() != 1 {
            return Err(LifecycleError::InvalidState);
        }
        let audit_id = audit(
            &mut tx,
            "lifecycle.restore.reopen",
            "changed",
            witness.recovery_generation,
            "manifest",
            "committed",
        )
        .await?;
        sqlx::query("INSERT INTO restore_reviews (review_id,relay_id,witness_generation,manifest_digest,action,audit_id) VALUES ($1,$2,$3,$4,'reopen',$5)")
            .bind(Uuid::now_v7()).bind(&witness.relay_id).bind(witness.recovery_generation).bind(reviewed.digest().as_slice()).bind(audit_id)
            .execute(&mut *tx).await.map_err(|_error| LifecycleError::Durable)?;
        tx.commit()
            .await
            .map_err(|_error| LifecycleError::Durable)?;
        self.witness
            .complete_recovery_review(witness, &review_digest)
            .map_err(Into::into)
    }

    /// Completes a locally interrupted reviewed reopen using only durable evidence.
    ///
    /// # Errors
    /// Returns an error unless the active witness, committed audited review, and
    /// normalized current authority state all describe the same recovery review.
    pub async fn resume_reopen_local(&self) -> Result<WitnessRecord, LifecycleError> {
        let witness = self.witness.latest()?.ok_or(LifecycleError::InvalidState)?;
        if !witness.active_run || !witness.recovery_pending_review {
            return Err(LifecycleError::InvalidState);
        }
        let mut tx = self.begin().await?;
        self.store
            .lock_relay_identity_in_transaction(&mut tx, &witness.relay_id)
            .await
            .map_err(|_error| LifecycleError::InvalidState)?;
        self.require_no_live_lease(&mut tx, &witness.relay_id)
            .await?;
        quarantine_review_matches(&mut tx, &witness).await?;
        let incidents = self.witness.incident_review_at(&witness)?;
        let state: String =
            sqlx::query_scalar("SELECT state FROM relay_identity WHERE relay_id=$1")
                .bind(&witness.relay_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(|_error| LifecycleError::Durable)?;
        if state != "normal" {
            return Err(LifecycleError::InvalidState);
        }
        let committed_digest = committed_reopen_digest(&mut tx, &witness)
            .await?
            .ok_or(LifecycleError::InvalidState)?;
        let computed = semantic_manifest(&mut tx, incidents, true).await?;
        if hex::encode(computed.digest()) != committed_digest
            || witness.incident_digest != hex::encode(computed.incident_digest())
            || unreconciled_incidents(
                &mut tx,
                &witness.relay_id,
                witness.recovery_generation,
                computed.incidents(),
            )
            .await?
            || unreconciled_host_state(&mut tx).await?
        {
            return Err(LifecycleError::ManifestMismatch);
        }
        tx.commit()
            .await
            .map_err(|_error| LifecycleError::Durable)?;
        self.witness
            .complete_recovery_review(&witness, &committed_digest)
            .map_err(Into::into)
    }

    async fn begin(&self) -> Result<Transaction<'_, Postgres>, LifecycleError> {
        self.store
            .begin_serializable()
            .await
            .map_err(|_error| LifecycleError::Durable)
    }

    async fn require_no_live_lease(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        relay_id: &str,
    ) -> Result<(), LifecycleError> {
        let live: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM relay_lease WHERE relay_id=$1 AND expires_at > clock_timestamp())",
        )
        .bind(relay_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|_error| LifecycleError::Durable)?;
        if live {
            Err(LifecycleError::InvalidState)
        } else {
            Ok(())
        }
    }
}

pub(crate) fn validate_bootstrap(request: &BootstrapRequest) -> Result<(), LifecycleError> {
    if request.relay_id.is_empty()
        || request.issuer.is_empty()
        || request.subject.is_empty()
        || request.relay_id.contains('\0')
        || request.issuer.contains('\0')
        || request.subject.contains('\0')
        || request.relay_id.len() > MAX_IDENTITY_BYTES
        || request.issuer.len() > MAX_IDENTITY_BYTES
        || request.subject.len() > MAX_IDENTITY_BYTES
    {
        return Err(LifecycleError::InvalidState);
    }
    Ok(())
}

async fn invalidate_old_generation(
    tx: &mut Transaction<'_, Postgres>,
    relay_id: &str,
    generation: i64,
) -> Result<(), LifecycleError> {
    for statement in [
        "UPDATE browser_sessions SET revoked_at=clock_timestamp(),session_generation=session_generation+1 WHERE revoked_at IS NULL",
        "UPDATE relay_credentials SET revoked_at=clock_timestamp(),credential_generation=credential_generation+1 WHERE revoked_at IS NULL",
        "UPDATE browser_logins SET consumed_at=clock_timestamp(),outcome='cancelled' WHERE consumed_at IS NULL",
        "UPDATE device_logins SET consumed_at=clock_timestamp(),outcome='cancelled',poll_lease_until=NULL WHERE consumed_at IS NULL",
        "UPDATE account_link_transactions SET completed_at=clock_timestamp() WHERE completed_at IS NULL",
        "UPDATE evidence_challenges SET consumed_at=clock_timestamp() WHERE consumed_at IS NULL",
        "UPDATE teams SET policy_generation=policy_generation+1,revision=revision+1,updated_at=clock_timestamp()",
        "DELETE FROM relay_lease",
    ] { sqlx::query(statement).execute(&mut **tx).await.map_err(|_error| LifecycleError::Durable)?; }
    sqlx::query("INSERT INTO revocations (revocation_id,scope_kind,scope_id,policy_generation,recovery_generation,reason_code,correlation_id) VALUES ($1,'recovery',$2,1,$3,'restore_quarantine',$4)")
        .bind(Uuid::now_v7()).bind(relay_id).bind(generation).bind(Uuid::now_v7()).execute(&mut **tx).await.map_err(|_error| LifecycleError::Durable)?;
    Ok(())
}

async fn record_global_overflow_reconciliation(
    tx: &mut Transaction<'_, Postgres>,
    relay_id: &str,
    generation: i64,
) -> Result<(), LifecycleError> {
    sqlx::query("INSERT INTO revocations (revocation_id,scope_kind,scope_id,policy_generation,recovery_generation,reason_code,correlation_id) VALUES ($1,'global',$2,1,$3,'restore_incident_overflow',$4)")
        .bind(Uuid::now_v7())
        .bind(relay_id)
        .bind(generation)
        .bind(Uuid::now_v7())
        .execute(&mut **tx)
        .await
        .map_err(|_error| LifecycleError::Durable)?;
    Ok(())
}

async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    action: &str,
    decision: &str,
    generation: i64,
    parameter: &str,
    outcome: &str,
) -> Result<Uuid, LifecycleError> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO audit_events (audit_id,actor_kind,action,decision,policy_generation,recovery_generation,correlation_id,parameter_code,parameter_value,outcome) VALUES ($1,'system',$2,$3,1,$4,$5,'lifecycle',$6,$7)")
        .bind(id).bind(action).bind(decision).bind(generation).bind(Uuid::now_v7()).bind(parameter).bind(outcome).execute(&mut **tx).await.map_err(|_error| LifecycleError::Durable)?;
    Ok(id)
}

async fn semantic_manifest(
    tx: &mut Transaction<'_, Postgres>,
    incidents: IncidentReview,
    normalize_reopen_identity: bool,
) -> Result<ReviewedManifest, LifecycleError> {
    const QUERIES: &[(ManifestKind, &[&str], &str)] = &[
        (ManifestKind::Principal, &["id", "kind", "state", "generation"], "SELECT id::text,kind::text,state::text,generation::text FROM principals ORDER BY id"),
        (ManifestKind::OidcIdentity, &["identity_id", "issuer", "subject", "principal_id", "link_generation", "removed_at"], "SELECT identity_id::text,issuer::text,subject::text,principal_id::text,link_generation::text,removed_at::text FROM oidc_identities ORDER BY identity_id"),
        (ManifestKind::Team, &["team_id", "state", "policy_generation", "revision"], "SELECT team_id::text,state::text,policy_generation::text,revision::text FROM teams ORDER BY team_id"),
        (ManifestKind::Membership, &["membership_id", "team_id", "principal_id", "builtin_role", "state", "revision", "local_deny_generation"], "SELECT membership_id::text,team_id::text,principal_id::text,builtin_role::text,state::text,revision::text,local_deny_generation::text FROM memberships ORDER BY membership_id"),
        (ManifestKind::CustomRole, &["team_id", "role_id", "display_name", "state", "revision"], "SELECT team_id::text,role_id::text,display_name::text,state::text,revision::text FROM custom_roles ORDER BY team_id,role_id"),
        (ManifestKind::CustomRolePermission, &["team_id", "role_id", "permission"], "SELECT team_id::text,role_id::text,permission::text FROM custom_role_permissions ORDER BY team_id,role_id,permission"),
        (ManifestKind::MembershipCustomRole, &["team_id", "principal_id", "role_id"], "SELECT team_id::text,principal_id::text,role_id::text FROM membership_custom_roles ORDER BY team_id,principal_id,role_id"),
        (ManifestKind::Group, &["team_id", "group_id", "state", "revision"], "SELECT team_id::text,group_id::text,state::text,revision::text FROM groups ORDER BY team_id,group_id"),
        (ManifestKind::GroupMember, &["team_id", "group_id", "principal_id", "revision"], "SELECT team_id::text,group_id::text,principal_id::text,revision::text FROM group_members ORDER BY team_id,group_id,principal_id"),
        (ManifestKind::Grant, &["team_id", "grant_id", "subject_kind", "subject_id", "resource_kind", "resource_id", "permission", "state", "revision"], "SELECT team_id::text,grant_id::text,subject_kind::text,subject_id::text,resource_kind::text,resource_id::text,permission::text,state::text,revision::text FROM grants ORDER BY team_id,grant_id"),
        (ManifestKind::HostOwnership, &["host_id", "relay_id", "owner_principal_id", "owner_team_id", "state"], "SELECT host_id::text,relay_id::text,owner_principal_id::text,owner_team_id::text,state::text FROM host_ownership_references ORDER BY host_id"),
        (ManifestKind::HistorySummary, &["revocation_count", "pending_cancellation_count"], "SELECT (SELECT count(*) FROM revocations)::text,(SELECT count(*) FROM cancellation_outbox WHERE delivered_at IS NULL)::text"),
        (ManifestKind::AccountLink, &["link_id", "principal_id", "source_identity_id", "account_link_generation", "recovery_generation", "expires_at", "completed_at"], "SELECT link_id::text,principal_id::text,source_identity_id::text,account_link_generation::text,recovery_generation::text,expires_at::text,completed_at::text FROM account_link_transactions ORDER BY link_id"),
        (ManifestKind::BrowserLogin, &["login_id", "issuer", "client_id", "audience", "redirect_uri", "action", "link_id", "account_link_generation", "recovery_generation", "expires_at", "consumed_at", "outcome"], "SELECT login_id::text,issuer::text,client_id::text,audience::text,redirect_uri::text,action::text,link_id::text,account_link_generation::text,recovery_generation::text,expires_at::text,consumed_at::text,outcome::text FROM browser_logins ORDER BY login_id"),
        (ManifestKind::DeviceLogin, &["login_id", "issuer", "client_id", "audience", "action", "link_id", "account_link_generation", "recovery_generation", "expires_at", "next_poll_at", "consumed_at", "outcome"], "SELECT login_id::text,issuer::text,client_id::text,audience::text,action::text,link_id::text,account_link_generation::text,recovery_generation::text,expires_at::text,next_poll_at::text,consumed_at::text,outcome::text FROM device_logins ORDER BY login_id"),
        (ManifestKind::BrowserSession, &["session_id", "principal_id", "identity_id", "digest_key_id", "session_generation", "recovery_generation", "expires_at", "idle_deadline", "revoked_at", "cookie_digest_commitment", "csrf_digest_commitment"], "SELECT session_id::text,principal_id::text,identity_id::text,digest_key_id::text,session_generation::text,recovery_generation::text,expires_at::text,idle_deadline::text,revoked_at::text,encode(sha256(convert_to('pohunek-recovery-manifest-v2/browser_sessions/cookie_digest/','UTF8') || uuid_send(session_id) || cookie_digest),'hex'),encode(sha256(convert_to('pohunek-recovery-manifest-v2/browser_sessions/csrf_digest/','UTF8') || uuid_send(session_id) || csrf_digest),'hex') FROM browser_sessions ORDER BY session_id"),
        (ManifestKind::Credential, &["credential_id", "public_id", "principal_id", "identity_id", "digest_key_id", "credential_kind", "credential_generation", "rotation_family_id", "predecessor_credential_id", "recovery_generation", "issued_at", "expires_at", "rotation_overlap_ends_at", "revoked_at", "secret_digest_commitment"], "SELECT credential_id::text,public_id::text,principal_id::text,identity_id::text,digest_key_id::text,credential_kind::text,credential_generation::text,rotation_family_id::text,predecessor_credential_id::text,recovery_generation::text,issued_at::text,expires_at::text,rotation_overlap_ends_at::text,revoked_at::text,encode(sha256(convert_to('pohunek-recovery-manifest-v2/relay_credentials/secret_digest/','UTF8') || uuid_send(credential_id) || secret_digest),'hex') FROM relay_credentials ORDER BY credential_id"),
        (ManifestKind::ServiceAccount, &["principal_id", "team_id", "display_name", "deprovisioned_at"], "SELECT principal_id::text,team_id::text,display_name::text,deprovisioned_at::text FROM service_accounts ORDER BY principal_id"),
        (ManifestKind::EvidenceChallenge, &["challenge_id", "relay_id", "principal_id", "audience", "team_id", "admission_rule_id", "admission_rule_revision", "account_link_generation", "recovery_generation", "expires_at", "consumed_at"], "SELECT challenge_id::text,relay_id::text,principal_id::text,audience::text,team_id::text,admission_rule_id::text,admission_rule_revision::text,account_link_generation::text,recovery_generation::text,expires_at::text,consumed_at::text FROM evidence_challenges ORDER BY challenge_id"),
        (ManifestKind::AdmissionRule, &["admission_rule_id", "team_id", "provider", "method", "match_value", "current_revision", "state"], "SELECT admission_rule_id::text,team_id::text,provider::text,method::text,match_value::text,current_revision::text,state::text FROM admission_rules ORDER BY admission_rule_id"),
        // Evidence is append-only and unbounded at rest, so it is committed into
        // one bounded summary row (count + SHA-256 of every security-relevant
        // field) instead of one manifest row per historical check. That keeps
        // recovery under the row/byte ceilings and still lets any tampering
        // change the reviewed digest.
        (ManifestKind::EvidenceResult, &["result_count", "result_digest"], "SELECT (SELECT count(*) FROM evidence_results)::text, encode(sha256(convert_to(coalesce(string_agg(evidence_id::text || '|' || challenge_id::text || '|' || principal_id::text || '|' || team_id::text || '|' || admission_rule_id::text || '|' || admission_rule_revision::text || '|' || provider::text || '|' || provider_subject::text || '|' || method::text || '|' || signing_key_id::text || '|' || outcome::text || '|' || checked_at::text || '|' || expires_at::text, E'\\n' ORDER BY evidence_id), ''), 'UTF8')), 'hex') FROM evidence_results"),
    ];
    // Timestamp text is authority evidence: connection/server locale changes
    // must not alter the representation of the same stored instant.
    sqlx::raw_sql("SET LOCAL TimeZone = 'UTC'; SET LOCAL DateStyle = 'ISO, YMD';")
        .execute(&mut **tx)
        .await
        .map_err(|_error| LifecycleError::Durable)?;

    let relay_query = if normalize_reopen_identity {
        "SELECT relay_id::text,recovery_generation::text,CASE WHEN state='normal' THEN 'recovery_quarantine' ELSE state END::text,CASE WHEN state='normal' THEN revision-1 ELSE revision END::text FROM relay_identity ORDER BY relay_id"
    } else {
        "SELECT relay_id::text,recovery_generation::text,state::text,revision::text FROM relay_identity ORDER BY relay_id"
    };
    let mut rows = Vec::new();
    let mut manifest_bytes = manifest_query_bytes(&mut *tx, relay_query).await?;
    if manifest_bytes > MAX_MANIFEST_BYTES {
        return Err(LifecycleError::Durable);
    }
    let relay_rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{relay_query} LIMIT {}",
        MAX_MANIFEST_ROWS + 1
    )))
    .fetch_all(&mut **tx)
    .await
    .map_err(|_error| LifecycleError::Durable)?;
    if relay_rows.len() > MAX_MANIFEST_ROWS {
        return Err(LifecycleError::Durable);
    }
    for row in relay_rows {
        rows.push(manifest_row(
            ManifestKind::RelayIdentity,
            &["relay_id", "recovery_generation", "state", "revision"],
            &row,
        )?);
    }
    for (kind, names, query) in QUERIES {
        manifest_bytes = manifest_bytes
            .checked_add(manifest_query_bytes(&mut *tx, query).await?)
            .ok_or(LifecycleError::Durable)?;
        if manifest_bytes > MAX_MANIFEST_BYTES {
            return Err(LifecycleError::Durable);
        }
        let result = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{query} LIMIT {}",
            MAX_MANIFEST_ROWS + 1
        )))
        .fetch_all(&mut **tx)
        .await
        .map_err(|_error| LifecycleError::Durable)?;
        if result.len() > MAX_MANIFEST_ROWS
            || rows.len().saturating_add(result.len()) > MAX_MANIFEST_ROWS
        {
            return Err(LifecycleError::Durable);
        }
        for row in result {
            rows.push(manifest_row(*kind, names, &row)?);
        }
    }
    // Query order need not mirror enum order. Preserve each query's primary-key
    // order while canonicalizing the relation order for all populated tables.
    rows.sort_by_key(|row| row.kind);
    ReviewedManifest::from_rows(rows, &incidents)
}

async fn manifest_query_bytes(
    tx: &mut Transaction<'_, Postgres>,
    query: &str,
) -> Result<i64, LifecycleError> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COALESCE(sum(octet_length(row_to_json(manifest_row)::text)), 0) FROM ({query}) AS manifest_row"
    )))
    .fetch_one(&mut **tx)
    .await
    .map_err(|_error| LifecycleError::Durable)
}

fn manifest_row(
    kind: ManifestKind,
    names: &'static [&'static str],
    row: &sqlx::postgres::PgRow,
) -> Result<ManifestRow, LifecycleError> {
    let fields = names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            Ok::<ManifestField, LifecycleError>(ManifestField {
                name,
                value: row
                    .try_get::<Option<String>, _>(index)
                    .map_err(|_error| LifecycleError::Durable)?,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ManifestRow { kind, fields })
}

async fn unreconciled_host_state(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<bool, LifecycleError> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM host_ownership_references)")
        .fetch_one(&mut **tx)
        .await
        .map_err(|_error| LifecycleError::Durable)
}

async fn unreconciled_incidents(
    tx: &mut Transaction<'_, Postgres>,
    relay_id: &str,
    generation: i64,
    incidents: &[DenyIncident],
) -> Result<bool, LifecycleError> {
    for incident in incidents {
        let reconciled = match incident {
            DenyIncident::Global { relay_id: incident_relay_id }
            | DenyIncident::Recovery {
                relay_id: incident_relay_id,
            } => {
                let scope_kind = if matches!(incident, DenyIncident::Global { .. }) {
                    "global"
                } else {
                    "recovery"
                };
                sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (SELECT 1 FROM revocations AS revocation JOIN relay_identity AS relay ON relay.relay_id=$2 WHERE revocation.scope_kind=$1 AND revocation.scope_id=$2 AND revocation.recovery_generation=relay.recovery_generation AND revocation.recovery_generation=$3 AND relay.relay_id=$4)",
                )
                .bind(scope_kind)
                .bind(incident_relay_id)
                .bind(generation)
                .bind(relay_id)
                .fetch_one(&mut **tx)
                .await
            }
            DenyIncident::Team { team_id } => sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM revocations WHERE scope_kind='team' AND scope_id=$1::text) AND EXISTS (SELECT 1 FROM teams WHERE team_id=$1 AND state='disabled')",
            )
            .bind(team_id)
            .fetch_one(&mut **tx)
            .await,
            DenyIncident::Principal { principal_id } => sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM revocations WHERE scope_kind='principal' AND scope_id=$1::text) AND EXISTS (SELECT 1 FROM principals WHERE id=$1 AND state='deprovisioned')",
            )
            .bind(principal_id)
            .fetch_one(&mut **tx)
            .await,
            DenyIncident::Credential { credential_id } => sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM revocations WHERE scope_kind='credential' AND scope_id=$1::text) AND EXISTS (SELECT 1 FROM relay_credentials WHERE credential_id=$1 AND revoked_at IS NOT NULL)",
            )
            .bind(credential_id)
            .fetch_one(&mut **tx)
            .await,
            DenyIncident::Membership { membership_id } => sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM revocations WHERE scope_kind='membership' AND scope_id=$1::text) AND EXISTS (SELECT 1 FROM memberships WHERE membership_id=$1 AND state='removed')",
            )
            .bind(membership_id)
            .fetch_one(&mut **tx)
            .await,
            DenyIncident::Policy { team_id } => sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM revocations WHERE scope_kind='policy' AND scope_id=$1::text)",
            )
            .bind(team_id)
            .fetch_one(&mut **tx)
            .await,
        }
        .map_err(|_error| LifecycleError::Durable)?;
        if !reconciled {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn quarantine_review_matches(
    tx: &mut Transaction<'_, Postgres>,
    witness: &WitnessRecord,
) -> Result<(), LifecycleError> {
    let matches = sqlx::query_scalar::<_, Option<bool>>(
        "SELECT review.manifest_digest=decode($3,'hex') AND audit.action='lifecycle.restore.quarantine' AND audit.decision='revoke' AND audit.recovery_generation=$2 AND audit.outcome='committed' FROM restore_reviews AS review LEFT JOIN audit_events AS audit ON audit.audit_id=review.audit_id WHERE review.relay_id=$1 AND review.witness_generation=$2 AND review.action='quarantine' LIMIT 2",
    )
    .bind(&witness.relay_id)
    .bind(witness.recovery_generation)
    .bind(&witness.manifest_digest)
    .fetch_all(&mut **tx)
    .await
    .map_err(|_error| LifecycleError::Durable)?;
    if matches.as_slice() == [Some(true)] {
        Ok(())
    } else {
        Err(LifecycleError::InvalidState)
    }
}

async fn reopen_review_matches(
    tx: &mut Transaction<'_, Postgres>,
    witness: &WitnessRecord,
    reviewed: &ReviewedManifest,
) -> Result<bool, LifecycleError> {
    Ok(committed_reopen_digest(tx, witness)
        .await?
        .is_some_and(|digest| digest.as_bytes() == hex::encode(reviewed.digest()).as_bytes()))
}

async fn committed_reopen_digest(
    tx: &mut Transaction<'_, Postgres>,
    witness: &WitnessRecord,
) -> Result<Option<String>, LifecycleError> {
    // Only zero, one, or multiple receipts matter; restored rows remain untrusted.
    let digests = sqlx::query_scalar::<_, String>(
        "SELECT encode(review.manifest_digest, 'hex') FROM restore_reviews AS review JOIN audit_events AS audit ON audit.audit_id=review.audit_id WHERE review.relay_id=$1 AND review.witness_generation=$2 AND review.action='reopen' AND audit.action='lifecycle.restore.reopen' AND audit.decision='changed' AND audit.recovery_generation=$2 AND audit.outcome='committed' LIMIT 2",
    )
    .bind(&witness.relay_id)
    .bind(witness.recovery_generation)
    .fetch_all(&mut **tx)
    .await
    .map_err(|_error| LifecycleError::Durable)?;
    match digests.as_slice() {
        [] => Ok(None),
        [digest] if digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
            Ok(Some(digest.clone()))
        }
        _ => Err(LifecycleError::InvalidState),
    }
}

#[cfg(all(test, feature = "postgres-tests"))]
mod tests {
    mod local;
    mod manifest;
    use std::{env, os::unix::fs::PermissionsExt};

    use ed25519_dalek::SigningKey;
    use sqlx::AssertSqlSafe;

    use super::*;

    const CONNECTIONS: u32 = 4;

    async fn fixture() -> (
        Store,
        String,
        sqlx::PgPool,
        Arc<WitnessStore>,
        tempfile::TempDir,
    ) {
        let url = env::var("POHUNEK_RELAY_TEST_DATABASE_URL")
            .expect("postgres-tests requires POHUNEK_RELAY_TEST_DATABASE_URL");
        let bootstrap = Store::connect(&url, 1).await.expect("connect PostgreSQL");
        let schema = format!("relay_lifecycle_{}", Uuid::now_v7().simple());
        sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(bootstrap.pool())
            .await
            .expect("create schema");
        let store = Store::connect(&format!("{url}?options[search_path]={schema}"), CONNECTIONS)
            .await
            .expect("connect scoped schema");
        store.migrate().await.expect("apply embedded migrations");
        let directory = tempfile::tempdir().expect("witness directory");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private witness directory");
        let witness = Arc::new(
            WitnessStore::open(
                directory.path(),
                SigningKey::from_bytes(&[7; 32]),
                "test".into(),
            )
            .expect("open witness"),
        );
        (store, schema, bootstrap.pool().clone(), witness, directory)
    }

    async fn cleanup(pool: &sqlx::PgPool, schema: &str) {
        sqlx::query(AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(pool)
            .await
            .expect("drop schema");
    }

    fn request() -> BootstrapRequest {
        BootstrapRequest {
            relay_id: "relay-test".into(),
            issuer: "https://issuer.test".into(),
            subject: "operator".into(),
        }
    }

    async fn insert_global_revocation(store: &Store, scope_id: &str, generation: i64) {
        sqlx::query("INSERT INTO revocations (revocation_id,scope_kind,scope_id,policy_generation,recovery_generation,reason_code,correlation_id) VALUES ($1,'global',$2,1,$3,'restore_incident_overflow',$4)")
            .bind(Uuid::now_v7())
            .bind(scope_id)
            .bind(generation)
            .bind(Uuid::now_v7())
            .execute(store.pool())
            .await
            .expect("insert global reconciliation marker");
    }

    fn overflow_checkpoint(witness: &WitnessStore, clean: &WitnessRecord) -> WitnessRecord {
        const OVERFLOW_INCIDENTS: usize = 257;
        let mut current = witness
            .begin_run(Some(clean), "relay-test", clean.recovery_generation)
            .expect("begin witnessed mutation run");
        for _ in 0..OVERFLOW_INCIDENTS {
            current = witness
                .record_deny_incident(
                    &current,
                    DenyIncident::Principal {
                        principal_id: Uuid::now_v7(),
                    },
                )
                .expect("append bounded deny incident");
        }
        witness
            .end_run(&current)
            .expect("end witnessed mutation run")
    }

    #[tokio::test]
    async fn bootstrap_creates_only_infrastructure_identity_and_cannot_repeat() {
        let (store, schema, pool, witness, _directory) = fixture().await;
        let lifecycle = Lifecycle::new(store.clone(), witness);
        lifecycle
            .bootstrap_local(request())
            .await
            .expect("bootstrap");
        let kinds: Vec<String> = sqlx::query_scalar("SELECT kind FROM principals ORDER BY id")
            .fetch_all(store.pool())
            .await
            .expect("read principals");
        let teams: i64 = sqlx::query_scalar("SELECT count(*) FROM teams")
            .fetch_one(store.pool())
            .await
            .expect("read teams");
        let grants: i64 = sqlx::query_scalar("SELECT count(*) FROM grants")
            .fetch_one(store.pool())
            .await
            .expect("read grants");
        assert_eq!(kinds, ["infrastructure"]);
        assert_eq!(teams, 0);
        assert_eq!(grants, 0);
        lifecycle
            .bootstrap_local(request())
            .await
            .expect("exact completed bootstrap replay");
        let mut different = request();
        different.subject = "different-subject".into();
        assert!(matches!(
            lifecycle.bootstrap_local(different).await,
            Err(LifecycleError::InvalidState)
        ));
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM principals")
                .fetch_one(store.pool())
                .await
                .expect("count principals"),
            1
        );
        cleanup(&pool, &schema).await;
    }

    #[tokio::test]
    async fn restart_cleanup_requires_current_fence_and_preserves_credential_generation() {
        let (store, schema, pool, witness, _directory) = fixture().await;
        let lifecycle = Lifecycle::new(store.clone(), witness);
        let clean = lifecycle
            .bootstrap_local(request())
            .await
            .expect("bootstrap");
        let generation = lifecycle
            .validate_normal_start(&clean)
            .await
            .expect("validate read only");
        let principal: Uuid = sqlx::query_scalar("SELECT id FROM principals")
            .fetch_one(store.pool())
            .await
            .expect("bootstrap principal");
        sqlx::query("INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,credential_kind,credential_generation,recovery_generation,expires_at,identity_id,digest_key_id,rotation_family_id) SELECT $1,$1::text,decode(repeat('01',32),'hex'),$2,'human',1,1,clock_timestamp() + interval '1 hour',identity_id,'test',$1 FROM oidc_identities WHERE principal_id=$2")
            .bind(Uuid::now_v7())
            .bind(principal)
            .execute(store.pool())
            .await
            .expect("seed credential");
        sqlx::query("INSERT INTO browser_logins (login_id,state_digest,nonce_digest,pkce_verifier_digest,login_binding_digest,issuer,client_id,audience,redirect_uri,action,account_link_generation,recovery_generation,expires_at) VALUES ($1,decode(repeat('02',32),'hex'),decode(repeat('03',32),'hex'),decode(repeat('04',32),'hex'),decode(repeat('05',32),'hex'),'https://issuer.test','client','audience','http://127.0.0.1/callback','login',1,1,clock_timestamp() + interval '1 hour')")
            .bind(Uuid::now_v7())
            .execute(store.pool())
            .await
            .expect("seed browser login");
        sqlx::query("INSERT INTO device_logins (login_id,device_code_digest,poll_secret_digest,issuer,client_id,audience,action,account_link_generation,recovery_generation,expires_at,poll_interval_seconds,next_poll_at) VALUES ($1,decode(repeat('06',32),'hex'),decode(repeat('07',32),'hex'),'https://issuer.test','client','audience','login',1,1,clock_timestamp() + interval '1 hour',5,clock_timestamp())")
            .bind(Uuid::now_v7())
            .execute(store.pool())
            .await
            .expect("seed device login");
        let lease = store
            .acquire_lease("relay-test", Uuid::now_v7(), generation)
            .await
            .expect("acquire lease");
        lifecycle
            .finalize_normal_start(&lease)
            .await
            .expect("fenced cleanup");
        let credential_generation: i64 =
            sqlx::query_scalar("SELECT credential_generation FROM relay_credentials")
                .fetch_one(store.pool())
                .await
                .expect("read credential");
        let pending: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM browser_logins WHERE consumed_at IS NULL) + (SELECT count(*) FROM device_logins WHERE consumed_at IS NULL)",
        )
        .fetch_one(store.pool())
        .await
        .expect("read pending logins");
        assert_eq!(credential_generation, 1);
        assert_eq!(pending, 0);
        cleanup(&pool, &schema).await;
    }

    #[tokio::test]
    async fn semantic_manifest_and_host_state_gate_reopen() {
        let (store, schema, pool, witness, _directory) = fixture().await;
        let lifecycle = Lifecycle::new(store.clone(), witness);
        let clean = lifecycle
            .bootstrap_local(request())
            .await
            .expect("bootstrap");
        let manifest = lifecycle
            .snapshot_authority_manifest()
            .await
            .expect("manifest");
        let advanced = lifecycle
            .advance_restore_witness(&clean, &manifest)
            .expect("advance witness");
        lifecycle
            .quarantine_restored(&advanced, &manifest)
            .await
            .expect("quarantine");
        let changed = lifecycle
            .snapshot_authority_manifest()
            .await
            .expect("post quarantine manifest");
        assert_ne!(manifest, changed);
        assert!(matches!(
            lifecycle.reopen_local(&advanced, &manifest).await,
            Err(LifecycleError::ManifestMismatch)
        ));
        cleanup(&pool, &schema).await;
    }

    #[tokio::test]
    async fn normal_start_rejects_historical_dirty_and_database_mismatch_witnesses() {
        let (store, schema, pool, witness, _directory) = fixture().await;
        let lifecycle = Lifecycle::new(store.clone(), Arc::clone(&witness));
        let initial = lifecycle
            .bootstrap_local(request())
            .await
            .expect("bootstrap");
        let active = witness
            .begin_run(Some(&initial), "relay-test", 1)
            .expect("dirty witness");
        assert!(matches!(
            lifecycle.validate_normal_start(&initial).await,
            Err(LifecycleError::InvalidState)
        ));
        assert!(matches!(
            lifecycle.validate_normal_start(&active).await,
            Err(LifecycleError::InvalidState)
        ));
        let clean = witness.end_run(&active).expect("clean witness");
        sqlx::query("UPDATE relay_identity SET recovery_generation=2 WHERE relay_id='relay-test'")
            .execute(store.pool())
            .await
            .expect("change database generation");
        assert!(matches!(
            lifecycle.validate_normal_start(&clean).await,
            Err(LifecycleError::InvalidState)
        ));
        cleanup(&pool, &schema).await;
    }

    #[tokio::test]
    async fn semantic_same_id_mutation_invalidates_review_and_reopen_stays_dirty() {
        let (store, schema, pool, witness, _directory) = fixture().await;
        let lifecycle = Lifecycle::new(store.clone(), Arc::clone(&witness));
        let clean = lifecycle
            .bootstrap_local(request())
            .await
            .expect("bootstrap");
        let before_restore = lifecycle
            .snapshot_authority_manifest()
            .await
            .expect("snapshot before restore");
        let advanced = lifecycle
            .advance_restore_witness(&clean, &before_restore)
            .expect("advance witness");
        lifecycle
            .quarantine_restored(&advanced, &before_restore)
            .await
            .expect("quarantine");
        let reviewed = lifecycle
            .snapshot_authority_manifest()
            .await
            .expect("review snapshot");
        sqlx::query("UPDATE principals SET generation=generation+1")
            .execute(store.pool())
            .await
            .expect("mutate same principal id");
        assert!(matches!(
            lifecycle.reopen_local(&advanced, &reviewed).await,
            Err(LifecycleError::ManifestMismatch)
        ));
        assert!(
            witness
                .latest()
                .expect("latest witness")
                .expect("advanced witness")
                .active_run
        );
        cleanup(&pool, &schema).await;
    }

    #[tokio::test]
    async fn audit_failure_rolls_back_quarantine_and_retains_dirty_witness() {
        let (store, schema, pool, witness, _directory) = fixture().await;
        let lifecycle = Lifecycle::new(store.clone(), Arc::clone(&witness));
        let clean = lifecycle
            .bootstrap_local(request())
            .await
            .expect("bootstrap");
        let manifest = lifecycle
            .snapshot_authority_manifest()
            .await
            .expect("snapshot");
        let advanced = lifecycle
            .advance_restore_witness(&clean, &manifest)
            .expect("advance witness");
        sqlx::raw_sql(AssertSqlSafe("CREATE FUNCTION reject_recovery_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'audit unavailable'; END; $$; CREATE TRIGGER reject_recovery_audit BEFORE INSERT ON audit_events FOR EACH ROW WHEN (NEW.action = 'lifecycle.restore.quarantine') EXECUTE FUNCTION reject_recovery_audit();".to_owned()))
            .execute(store.pool())
            .await
            .expect("install audit rejection");
        assert!(matches!(
            lifecycle.quarantine_restored(&advanced, &manifest).await,
            Err(LifecycleError::Durable)
        ));
        let state: String = sqlx::query_scalar("SELECT state FROM relay_identity")
            .fetch_one(store.pool())
            .await
            .expect("read unchanged state");
        assert_eq!(state, "normal");
        assert!(
            witness
                .latest()
                .expect("latest witness")
                .expect("advanced witness")
                .active_run
        );
        cleanup(&pool, &schema).await;
    }

    #[tokio::test]
    async fn stale_fence_cannot_cancel_a_successors_pending_login() {
        let (store, schema, pool, witness, _directory) = fixture().await;
        let lifecycle = Lifecycle::new(store.clone(), witness);
        let clean = lifecycle
            .bootstrap_local(request())
            .await
            .expect("bootstrap");
        sqlx::query("INSERT INTO browser_logins (login_id,state_digest,nonce_digest,pkce_verifier_digest,login_binding_digest,issuer,client_id,audience,redirect_uri,action,account_link_generation,recovery_generation,expires_at) VALUES ($1,decode(repeat('12',32),'hex'),decode(repeat('13',32),'hex'),decode(repeat('14',32),'hex'),decode(repeat('15',32),'hex'),'https://issuer.test','client','audience','http://127.0.0.1/callback','login',1,1,clock_timestamp() + interval '1 hour')")
            .bind(Uuid::now_v7())
            .execute(store.pool())
            .await
            .expect("seed browser login");
        let first = store
            .acquire_lease("relay-test", Uuid::now_v7(), clean.recovery_generation)
            .await
            .expect("first fence");
        store
            .release_lease(&first)
            .await
            .expect("release first fence");
        let second = store
            .acquire_lease("relay-test", Uuid::now_v7(), clean.recovery_generation)
            .await
            .expect("successor fence");
        let heartbeat: i64 = sqlx::query_scalar("SELECT heartbeat_sequence FROM relay_lease")
            .fetch_one(store.pool())
            .await
            .expect("read successor heartbeat");
        assert_eq!(heartbeat, 2);
        assert!(matches!(
            store.release_lease(&first).await,
            Err(crate::store::StoreError::StaleState)
        ));
        store.validate_lease(&second).await.unwrap();
        assert!(matches!(
            lifecycle.finalize_normal_start(&first).await,
            Err(LifecycleError::InvalidState)
        ));
        let pending: i64 =
            sqlx::query_scalar("SELECT count(*) FROM browser_logins WHERE consumed_at IS NULL")
                .fetch_one(store.pool())
                .await
                .expect("read preserved pending login");
        assert_eq!(pending, 1);
        cleanup(&pool, &schema).await;
    }

    #[tokio::test]
    async fn reviewed_recovery_reopens_and_subsequent_normal_start_succeeds() {
        let (store, schema, pool, witness, directory) = fixture().await;
        let lifecycle = Lifecycle::new(store.clone(), witness);
        let clean = lifecycle
            .bootstrap_local(request())
            .await
            .expect("bootstrap");
        let before_restore = lifecycle
            .snapshot_authority_manifest()
            .await
            .expect("snapshot before restore");
        let advanced = lifecycle
            .advance_restore_witness(&clean, &before_restore)
            .expect("advance witness");
        lifecycle
            .quarantine_restored(&advanced, &before_restore)
            .await
            .expect("quarantine");
        let reviewed = lifecycle
            .snapshot_authority_manifest()
            .await
            .expect("review snapshot");
        let completed = lifecycle
            .reopen_local(&advanced, &reviewed)
            .await
            .expect("reopen");
        assert!(!directory.path().join("witness.active").exists());
        assert_eq!(
            lifecycle
                .reopen_local(&advanced, &reviewed)
                .await
                .expect("retry lost reopen response"),
            completed
        );
        assert!(!completed.active_run);
        assert!(!completed.recovery_pending_review);
        assert_eq!(
            lifecycle
                .validate_normal_start(&completed)
                .await
                .expect("normal start"),
            completed.recovery_generation
        );
        sqlx::query("UPDATE principals SET generation=generation+1")
            .execute(store.pool())
            .await
            .expect("mutate authority after completed recovery");
        assert!(matches!(
            lifecycle.reopen_local(&advanced, &reviewed).await,
            Err(LifecycleError::ManifestMismatch)
        ));
        cleanup(&pool, &schema).await;
    }

    #[tokio::test]
    async fn witnessed_denial_blocks_a_pre_revocation_database_restore() {
        let (store, schema, pool, witness, _directory) = fixture().await;
        let lifecycle = Lifecycle::new(store.clone(), Arc::clone(&witness));
        let clean = lifecycle
            .bootstrap_local(request())
            .await
            .expect("bootstrap");
        let principal: Uuid = sqlx::query_scalar("SELECT id FROM principals")
            .fetch_one(store.pool())
            .await
            .expect("bootstrap principal");
        let denied = witness
            .record_deny_incident(
                &clean,
                DenyIncident::Principal {
                    principal_id: principal,
                },
            )
            .expect("witness denial");
        let checkpoint = witness.end_run(&denied).expect("clean denial witness");
        let manifest = lifecycle
            .snapshot_authority_manifest()
            .await
            .expect("incident review manifest");
        assert_eq!(
            manifest.incidents(),
            &[DenyIncident::Principal {
                principal_id: principal
            }]
        );
        let advanced = lifecycle
            .advance_restore_witness(&checkpoint, &manifest)
            .expect("advance witness");
        lifecycle
            .quarantine_restored(&advanced, &manifest)
            .await
            .expect("quarantine restored database");
        let reviewed = lifecycle
            .snapshot_authority_manifest()
            .await
            .expect("post quarantine review");
        assert!(matches!(
            lifecycle.reopen_local(&advanced, &reviewed).await,
            Err(LifecycleError::ManifestMismatch)
        ));
        cleanup(&pool, &schema).await;
    }

    #[tokio::test]
    async fn resume_reopen_uses_only_durable_state_after_witness_write_failure() {
        let (store, schema, pool, witness, directory) = fixture().await;
        let lifecycle = Lifecycle::new(store.clone(), Arc::clone(&witness));
        let clean = lifecycle
            .bootstrap_local(request())
            .await
            .expect("bootstrap");
        let before = lifecycle
            .snapshot_authority_manifest()
            .await
            .expect("before restore manifest");
        let advanced = lifecycle
            .advance_restore_witness(&clean, &before)
            .expect("advance witness");
        lifecycle
            .quarantine_restored(&advanced, &before)
            .await
            .expect("quarantine");
        let reviewed = lifecycle
            .snapshot_authority_manifest()
            .await
            .expect("review manifest");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o500))
            .expect("deny witness writes");
        assert!(matches!(
            lifecycle.reopen_local(&advanced, &reviewed).await,
            Err(LifecycleError::Witness(_))
        ));
        let state: String = sqlx::query_scalar("SELECT state FROM relay_identity")
            .fetch_one(store.pool())
            .await
            .expect("database reopen committed");
        assert_eq!(state, "normal");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("restore witness writes");
        drop(reviewed);
        drop(lifecycle);
        drop(witness);
        let resumed_witness = Arc::new(
            WitnessStore::open(
                directory.path(),
                SigningKey::from_bytes(&[7; 32]),
                "test".into(),
            )
            .expect("reopen witness after process loss"),
        );
        let resumed = Lifecycle::new(store.clone(), Arc::clone(&resumed_witness));
        sqlx::query("UPDATE principals SET generation=generation+1")
            .execute(store.pool())
            .await
            .expect("mutate reviewed authority after commit");
        assert!(matches!(
            resumed.resume_reopen_local().await,
            Err(LifecycleError::ManifestMismatch)
        ));
        sqlx::query("UPDATE principals SET generation=generation-1")
            .execute(store.pool())
            .await
            .expect("restore reviewed authority");
        let receipt_digest: String = sqlx::query_scalar(
            "SELECT encode(manifest_digest, 'hex') FROM restore_reviews WHERE relay_id='relay-test' AND witness_generation=$1 AND action='reopen'",
        )
        .bind(advanced.recovery_generation)
        .fetch_one(store.pool())
        .await
        .expect("read durable reopen receipt digest");
        sqlx::query("UPDATE restore_reviews SET manifest_digest=decode(repeat('ff',32),'hex') WHERE relay_id='relay-test' AND witness_generation=$1 AND action='reopen'")
            .bind(advanced.recovery_generation)
            .execute(store.pool())
            .await
            .expect("tamper committed review receipt");
        assert!(matches!(
            resumed.resume_reopen_local().await,
            Err(LifecycleError::ManifestMismatch)
        ));
        sqlx::query("UPDATE restore_reviews SET manifest_digest=decode($1,'hex') WHERE relay_id='relay-test' AND witness_generation=$2 AND action='reopen'")
            .bind(&receipt_digest)
            .bind(advanced.recovery_generation)
            .execute(store.pool())
            .await
            .expect("restore durable review receipt");
        let completed = resumed
            .resume_reopen_local()
            .await
            .expect("resume exact durable review");
        assert!(!completed.active_run);
        assert_eq!(
            resumed
                .validate_normal_start(&completed)
                .await
                .expect("normal start after resumed review"),
            completed.recovery_generation
        );
        cleanup(&pool, &schema).await;
    }

    #[tokio::test]
    async fn committed_reopen_lookup_rejects_duplicate_audited_receipts() {
        let (store, schema, pool, witness, _directory) = fixture().await;
        let lifecycle = Lifecycle::new(store.clone(), witness);
        let checkpoint = lifecycle
            .bootstrap_local(request())
            .await
            .expect("bootstrap");
        let mut tx = store
            .begin_serializable()
            .await
            .expect("receipt transaction");
        assert_eq!(
            committed_reopen_digest(&mut tx, &checkpoint)
                .await
                .expect("no receipt"),
            None
        );
        for count in 1..=3 {
            let audit_id = audit(
                &mut tx,
                "lifecycle.restore.reopen",
                "changed",
                1,
                "bounded",
                "committed",
            )
            .await
            .expect("linked audit");
            sqlx::query("INSERT INTO restore_reviews (review_id,relay_id,witness_generation,manifest_digest,action,audit_id) VALUES ($1,'relay-test',1,decode(repeat('ab',32),'hex'),'reopen',$2)")
                .bind(Uuid::now_v7()).bind(audit_id).execute(&mut *tx).await.expect("audited receipt");
            let found = committed_reopen_digest(&mut tx, &checkpoint).await;
            if count == 1 {
                assert_eq!(found.expect("sole receipt"), Some("ab".repeat(32)));
            } else {
                assert!(matches!(found, Err(LifecycleError::InvalidState)));
            }
        }
        tx.rollback().await.expect("remove fixture receipts");
        cleanup(&pool, &schema).await;
    }

    #[tokio::test]
    async fn resume_reopen_rejects_a_witness_successor_after_the_reviewed_child() {
        let (store, schema, pool, witness, directory) = fixture().await;
        let lifecycle = Lifecycle::new(store.clone(), Arc::clone(&witness));
        let clean = lifecycle
            .bootstrap_local(request())
            .await
            .expect("bootstrap");
        let before = lifecycle
            .snapshot_authority_manifest()
            .await
            .expect("before restore manifest");
        let advanced = lifecycle
            .advance_restore_witness(&clean, &before)
            .expect("advance witness");
        lifecycle
            .quarantine_restored(&advanced, &before)
            .await
            .expect("quarantine");
        let reviewed = lifecycle
            .snapshot_authority_manifest()
            .await
            .expect("review manifest");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o500))
            .expect("deny witness writes");
        assert!(matches!(
            lifecycle.reopen_local(&advanced, &reviewed).await,
            Err(LifecycleError::Witness(_))
        ));
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("restore witness writes");
        let reviewed_child = witness
            .complete_recovery_review(&advanced, &hex::encode(reviewed.digest()))
            .expect("publish exact reviewed child");
        let successor = witness
            .begin_run(
                Some(&reviewed_child),
                "relay-test",
                advanced.recovery_generation,
            )
            .expect("append successor after reviewed child");
        drop(reviewed);
        drop(lifecycle);
        drop(witness);
        let resumed_witness = Arc::new(
            WitnessStore::open(
                directory.path(),
                SigningKey::from_bytes(&[7; 32]),
                "test".into(),
            )
            .expect("reopen witness after process loss"),
        );
        let resumed = Lifecycle::new(store.clone(), resumed_witness);
        assert!(matches!(
            resumed.resume_reopen_local().await,
            Err(LifecycleError::InvalidState)
        ));
        assert!(!directory
            .path()
            .join(format!("witness.{:020}.json", successor.sequence + 1))
            .exists());
        cleanup(&pool, &schema).await;
    }

    #[tokio::test]
    async fn large_append_only_history_stays_bounded_for_review() {
        const HISTORY_ROWS: usize = MAX_MANIFEST_ROWS + 1;
        let (store, schema, pool, witness, _directory) = fixture().await;
        let lifecycle = Lifecycle::new(store.clone(), witness);
        lifecycle
            .bootstrap_local(request())
            .await
            .expect("bootstrap");
        let mut tx = store
            .begin_serializable()
            .await
            .expect("begin history insert");
        for _ in 0..HISTORY_ROWS {
            sqlx::query("INSERT INTO audit_events (audit_id,actor_kind,action,decision,policy_generation,recovery_generation,correlation_id,parameter_code,parameter_value,outcome) VALUES ($1,'system','history.test','changed',1,1,$2,'history','bounded','committed')")
                .bind(Uuid::now_v7())
                .bind(Uuid::now_v7())
                .execute(&mut *tx)
                .await
                .expect("append audit history");
        }
        tx.commit().await.expect("commit history");
        lifecycle.snapshot_authority_manifest().await.unwrap();
        cleanup(&pool, &schema).await;
    }

    #[tokio::test]
    async fn oversized_authority_text_is_rejected_before_manifest_values_are_loaded() {
        let (store, schema, pool, witness, _directory) = fixture().await;
        let lifecycle = Lifecycle::new(store.clone(), witness);
        lifecycle
            .bootstrap_local(request())
            .await
            .expect("bootstrap");
        sqlx::query("ALTER TABLE browser_logins DROP CONSTRAINT browser_logins_redirect_uri_check")
            .execute(store.pool())
            .await
            .expect("relax fixture-only redirect constraint");
        sqlx::query("INSERT INTO browser_logins (login_id,state_digest,nonce_digest,pkce_verifier_digest,login_binding_digest,issuer,client_id,audience,redirect_uri,action,account_link_generation,recovery_generation,expires_at) VALUES ($1,decode(repeat('01',32),'hex'),decode(repeat('02',32),'hex'),decode(repeat('03',32),'hex'),decode(repeat('04',32),'hex'),'issuer','client','client',repeat('x',$2::integer),'login',1,1,clock_timestamp()+interval '1 hour')")
            .bind(Uuid::now_v7())
            .bind(i32::try_from(MAX_MANIFEST_BYTES + 1).expect("manifest bound fits i32"))
            .execute(store.pool())
            .await
            .expect("seed oversized corrupt authority value");
        assert!(matches!(
            lifecycle.snapshot_authority_manifest().await,
            Err(LifecycleError::Durable)
        ));
        cleanup(&pool, &schema).await;
    }

    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "One recovery timeline verifies overflow, quarantine, and reviewed reopen together"
    )]
    async fn deny_history_overflow_quarantines_and_reopens_with_a_current_global_marker() {
        let (store, schema, pool, witness, directory) = fixture().await;
        let lifecycle = Lifecycle::new(store.clone(), Arc::clone(&witness));
        let clean = lifecycle
            .bootstrap_local(request())
            .await
            .expect("bootstrap");
        let checkpoint = overflow_checkpoint(&witness, &clean);
        let before = lifecycle
            .snapshot_authority_manifest()
            .await
            .expect("overflow is conservatively reviewable");
        assert_eq!(
            before.incidents(),
            &[DenyIncident::Global {
                relay_id: "relay-test".to_owned()
            }]
        );
        let advanced = lifecycle
            .advance_restore_witness(&checkpoint, &before)
            .expect("advance overflow review");
        assert!(matches!(
            lifecycle.reopen_local(&advanced, &before).await,
            Err(LifecycleError::InvalidState)
        ));
        lifecycle
            .quarantine_restored(&advanced, &before)
            .await
            .expect("quarantine overflow restore");
        let global_markers: i64 = sqlx::query_scalar("SELECT count(*) FROM revocations WHERE scope_kind='global' AND scope_id='relay-test' AND recovery_generation=$1 AND reason_code='restore_incident_overflow'")
            .bind(advanced.recovery_generation)
            .fetch_one(store.pool())
            .await
            .expect("read current global marker");
        assert_eq!(global_markers, 1);
        let recovery_markers: i64 = sqlx::query_scalar("SELECT count(*) FROM revocations WHERE scope_kind='recovery' AND scope_id='relay-test' AND recovery_generation=$1")
            .bind(advanced.recovery_generation)
            .fetch_one(store.pool())
            .await
            .expect("read recovery invalidation marker");
        assert_eq!(recovery_markers, 1);
        let reviewed = lifecycle
            .snapshot_authority_manifest()
            .await
            .expect("snapshot quarantined overflow authority");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o500))
            .expect("deny witness completion write");
        assert!(matches!(
            lifecycle.reopen_local(&advanced, &reviewed).await,
            Err(LifecycleError::Witness(_))
        ));
        let state: String = sqlx::query_scalar("SELECT state FROM relay_identity")
            .fetch_one(store.pool())
            .await
            .expect("read committed reopen state");
        assert_eq!(state, "normal");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("restore witness directory writes");
        let completed = lifecycle
            .reopen_local(&advanced, &reviewed)
            .await
            .expect("retry exact committed overflow review");
        assert_eq!(
            lifecycle
                .validate_normal_start(&completed)
                .await
                .expect("normal start after overflow recovery"),
            completed.recovery_generation
        );
        assert!(matches!(
            lifecycle.reopen_local(&advanced, &reviewed).await,
            Ok(retried) if retried == completed
        ));
        assert!(matches!(
            lifecycle.reopen_local(&advanced, &before).await,
            Err(LifecycleError::Witness(RecoveryError::StaleWitness))
        ));
        sqlx::query("DELETE FROM revocations WHERE scope_kind='global'")
            .execute(store.pool())
            .await
            .expect("remove current global marker");
        insert_global_revocation(&store, "other-relay", advanced.recovery_generation).await;
        assert!(matches!(
            lifecycle.reopen_local(&advanced, &reviewed).await,
            Err(LifecycleError::ManifestMismatch)
        ));
        sqlx::query("DELETE FROM revocations WHERE scope_kind='global'")
            .execute(store.pool())
            .await
            .expect("remove wrong relay marker");
        insert_global_revocation(&store, "relay-test", advanced.recovery_generation - 1).await;
        assert!(matches!(
            lifecycle.reopen_local(&advanced, &reviewed).await,
            Err(LifecycleError::ManifestMismatch)
        ));
        sqlx::query("DELETE FROM revocations WHERE scope_kind='global'")
            .execute(store.pool())
            .await
            .expect("remove old generation marker");
        insert_global_revocation(&store, "relay-test", advanced.recovery_generation).await;
        sqlx::query("UPDATE principals SET generation=generation+1")
            .execute(store.pool())
            .await
            .expect("mutate authority after completed review");
        assert!(matches!(
            lifecycle.reopen_local(&advanced, &reviewed).await,
            Err(LifecycleError::ManifestMismatch)
        ));
        cleanup(&pool, &schema).await;
    }

    #[tokio::test]
    async fn acquire_and_quarantine_serialize_on_the_identity_row() {
        let (store, schema, pool, witness, _directory) = fixture().await;
        let lifecycle = Lifecycle::new(store.clone(), Arc::clone(&witness));
        let clean = lifecycle
            .bootstrap_local(request())
            .await
            .expect("bootstrap");
        let manifest = lifecycle
            .snapshot_authority_manifest()
            .await
            .expect("manifest");
        let advanced = lifecycle
            .advance_restore_witness(&clean, &manifest)
            .expect("advance witness");
        let mut lock = store
            .begin_serializable()
            .await
            .expect("begin lock transaction");
        store
            .lock_relay_identity_in_transaction(&mut lock, "relay-test")
            .await
            .expect("lock relay identity");
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let acquire_store = store.clone();
        let acquire_barrier = Arc::clone(&barrier);
        let acquire = tokio::spawn(async move {
            acquire_barrier.wait().await;
            acquire_store
                .acquire_lease("relay-test", Uuid::now_v7(), clean.recovery_generation)
                .await
        });
        let quarantine_lifecycle = lifecycle.clone();
        let quarantine_barrier = Arc::clone(&barrier);
        let quarantine = tokio::spawn(async move {
            quarantine_barrier.wait().await;
            quarantine_lifecycle
                .quarantine_restored(&advanced, &manifest)
                .await
        });
        barrier.wait().await;
        lock.commit().await.expect("release identity lock");
        let acquired = acquire.await.expect("acquire task");
        let quarantined = quarantine.await.expect("quarantine task");
        assert_ne!(acquired.is_ok(), quarantined.is_ok());
        cleanup(&pool, &schema).await;
    }
}
