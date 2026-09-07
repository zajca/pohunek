//! Strict versioned identity and governance record codecs.
//!
//! Every object is decoded with an explicit visitor so duplicate fields and
//! unknown fields fail closed before governance state reaches the daemon.

// Rust guideline compliant 2026-09-04

use protocol::{
    ApprovalKeyReference, EnrollmentInfo, EnrollmentStatus, HostGovernanceStatus, HostId,
    HostOwner, OwnerRevision, QuarantineReason, SignedTransferOutcome,
};
use serde::{
    de::{MapAccess, Visitor},
    Deserialize, Deserializer, Serialize,
};

use super::repository::HostStateRepositoryError;

/// First identity format carrying durable bootstrap completion evidence.
const IDENTITY_FORMAT_VERSION: u8 = 2;
/// First and only supported governance JSON format.
/// Schema v2 adds private provenance needed for exact quarantine recovery.
const GOVERNANCE_FORMAT_VERSION: u8 = 2;
/// Identity contains a fixed-width opaque identifier, format marker, and phase.
pub(crate) const MAX_IDENTITY_RECORD_BYTES: usize = 1024;
/// Governance stores one bounded current state and one optional signed outcome.
pub(crate) const MAX_GOVERNANCE_RECORD_BYTES: usize = 64 * 1024;

/// Strict identity record held outside governance state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IdentityRecord {
    pub(crate) host_id: HostId,
    phase: IdentityBootstrapPhase,
}

impl IdentityRecord {
    pub(crate) const fn in_progress(host_id: HostId) -> Self {
        Self {
            host_id,
            phase: IdentityBootstrapPhase::InProgress,
        }
    }

    pub(crate) const fn complete(host_id: HostId) -> Self {
        Self {
            host_id,
            phase: IdentityBootstrapPhase::Complete,
        }
    }

    pub(crate) const fn is_in_progress(&self) -> bool {
        matches!(self.phase, IdentityBootstrapPhase::InProgress)
    }

    pub(crate) fn complete_record(&self) -> Self {
        Self::complete(self.host_id.clone())
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, HostStateRepositoryError> {
        let bytes = serde_json::to_vec(&IdentityWire {
            format_version: IDENTITY_FORMAT_VERSION,
            host_id: &self.host_id,
            phase: self.phase,
        })
        .map_err(|_error| HostStateRepositoryError::InvalidRecord {
            record: "identity.json",
        })?;
        if bytes.len() > MAX_IDENTITY_RECORD_BYTES {
            return Err(HostStateRepositoryError::RecordTooLarge {
                record: "identity.json",
                max_bytes: MAX_IDENTITY_RECORD_BYTES,
            });
        }
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, HostStateRepositoryError> {
        if bytes.len() > MAX_IDENTITY_RECORD_BYTES {
            return Err(HostStateRepositoryError::RecordTooLarge {
                record: "identity.json",
                max_bytes: MAX_IDENTITY_RECORD_BYTES,
            });
        }
        let wire: IdentityWireOwned = serde_json::from_slice(bytes).map_err(|_error| {
            HostStateRepositoryError::InvalidRecord {
                record: "identity.json",
            }
        })?;
        if wire.format_version != IDENTITY_FORMAT_VERSION {
            return Err(HostStateRepositoryError::UnsupportedRecordVersion {
                record: "identity.json",
            });
        }
        Ok(Self {
            host_id: wire.host_id,
            phase: wire.phase,
        })
    }
}

#[derive(Serialize)]
struct IdentityWire<'a> {
    format_version: u8,
    host_id: &'a HostId,
    phase: IdentityBootstrapPhase,
}

struct IdentityWireOwned {
    format_version: u8,
    host_id: HostId,
    phase: IdentityBootstrapPhase,
}

/// Durable bootstrap evidence for one identity record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum IdentityBootstrapPhase {
    /// Only an exact interrupted bootstrap prefix may continue.
    InProgress,
    /// All identity, approval-key, and governance records are complete.
    Complete,
}

impl<'de> Deserialize<'de> for IdentityWireOwned {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct IdentityVisitor;

        impl<'de> Visitor<'de> for IdentityVisitor {
            type Value = IdentityWireOwned;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an identity record object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut format_version: Option<u8> = None;
                let mut host_id: Option<HostId> = None;
                let mut phase: Option<IdentityBootstrapPhase> = None;
                while let Some(field) = map.next_key::<String>()? {
                    match field.as_str() {
                        "format_version" => {
                            set_once(&mut format_version, map.next_value()?, "format_version")?;
                        }
                        "host_id" => set_once(&mut host_id, map.next_value()?, "host_id")?,
                        "phase" => set_once(&mut phase, map.next_value()?, "phase")?,
                        _ => return Err(serde::de::Error::unknown_field(&field, IDENTITY_FIELDS)),
                    }
                }
                Ok(IdentityWireOwned {
                    format_version: format_version
                        .ok_or_else(|| serde::de::Error::missing_field("format_version"))?,
                    host_id: host_id.ok_or_else(|| serde::de::Error::missing_field("host_id"))?,
                    phase: phase.ok_or_else(|| serde::de::Error::missing_field("phase"))?,
                })
            }
        }

        deserializer.deserialize_struct("IdentityRecord", IDENTITY_FIELDS, IdentityVisitor)
    }
}

const IDENTITY_FIELDS: &[&str] = &["format_version", "host_id", "phase"];

/// Internal durable governance state including the one allowed outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GovernanceState {
    status: HostGovernanceStatus,
    /// Private lifecycle preserved for evidence-gated projection-conflict recovery.
    quarantine_origin: Option<EnrollmentStatus>,
    latest_transfer_outcome: Option<SignedTransferOutcome>,
    retired_enrollment: Option<RetiredEnrollment>,
}

/// One bounded preceding enrollment generation retained for reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetiredEnrollment {
    enrollment: EnrollmentInfo,
    owner: HostOwner,
    owner_revision: OwnerRevision,
    latest_transfer_outcome: Option<SignedTransferOutcome>,
}

impl RetiredEnrollment {
    /// Creates the only retained locally-unenrolled generation summary.
    pub(crate) fn new(
        enrollment: EnrollmentInfo,
        owner: HostOwner,
        owner_revision: OwnerRevision,
        latest_transfer_outcome: Option<SignedTransferOutcome>,
    ) -> Result<Self, HostStateRepositoryError> {
        if enrollment.status() != EnrollmentStatus::LocallyUnenrolled {
            return Err(HostStateRepositoryError::InvalidRecord {
                record: "governance.json",
            });
        }
        Ok(Self {
            enrollment,
            owner,
            owner_revision,
            latest_transfer_outcome,
        })
    }

    pub(crate) fn enrollment(&self) -> &EnrollmentInfo {
        &self.enrollment
    }

    pub(crate) fn owner(&self) -> &HostOwner {
        &self.owner
    }

    pub(crate) const fn owner_revision(&self) -> OwnerRevision {
        self.owner_revision
    }

    pub(crate) fn latest_transfer_outcome(&self) -> Option<&SignedTransferOutcome> {
        self.latest_transfer_outcome.as_ref()
    }
}

impl GovernanceState {
    pub(crate) fn initial(
        host_id: HostId,
        approval_key_reference: ApprovalKeyReference,
    ) -> Result<Self, HostStateRepositoryError> {
        Self::new(
            HostGovernanceStatus::new(host_id, None, None, None, None, approval_key_reference)
                .map_err(|_error| HostStateRepositoryError::InvalidRecord {
                    record: "governance.json",
                })?,
            None,
            None,
        )
    }

    pub(crate) fn new(
        status: HostGovernanceStatus,
        latest_transfer_outcome: Option<SignedTransferOutcome>,
        retired_enrollment: Option<RetiredEnrollment>,
    ) -> Result<Self, HostStateRepositoryError> {
        Self::with_quarantine_origin(status, None, latest_transfer_outcome, retired_enrollment)
    }

    /// Creates one state with private provenance for a quarantined lifecycle.
    pub(crate) fn with_quarantine_origin(
        status: HostGovernanceStatus,
        quarantine_origin: Option<EnrollmentStatus>,
        latest_transfer_outcome: Option<SignedTransferOutcome>,
        retired_enrollment: Option<RetiredEnrollment>,
    ) -> Result<Self, HostStateRepositoryError> {
        match (
            status.enrollment().map(EnrollmentInfo::status),
            quarantine_origin,
        ) {
            (Some(EnrollmentStatus::Quarantined), Some(origin))
                if origin != EnrollmentStatus::Quarantined => {}
            (Some(EnrollmentStatus::Quarantined), _) | (_, Some(_)) => {
                return Err(HostStateRepositoryError::InvalidRecord {
                    record: "governance.json",
                });
            }
            (_, None) => {}
        }
        if status.enrollment().is_none()
            && (latest_transfer_outcome.is_some() || retired_enrollment.is_some())
        {
            return Err(HostStateRepositoryError::InvalidRecord {
                record: "governance.json",
            });
        }
        if let Some(retired) = retired_enrollment.as_ref() {
            if retired.enrollment.status() != EnrollmentStatus::LocallyUnenrolled {
                return Err(HostStateRepositoryError::InvalidRecord {
                    record: "governance.json",
                });
            }
            let Some(current_enrollment) = status.enrollment() else {
                return Err(HostStateRepositoryError::InvalidRecord {
                    record: "governance.json",
                });
            };
            let Some(current_owner_revision) = status.owner_revision() else {
                return Err(HostStateRepositoryError::InvalidRecord {
                    record: "governance.json",
                });
            };
            if current_enrollment.revision().get() <= retired.enrollment.revision().get()
                || current_owner_revision.get() <= retired.owner_revision.get()
            {
                return Err(HostStateRepositoryError::InvalidRecord {
                    record: "governance.json",
                });
            }
            if let Some(outcome) = retired.latest_transfer_outcome() {
                validate_outcome_coordinates(outcome, &retired_status(&status, retired)?)?;
            }
        }
        if let Some(outcome) = latest_transfer_outcome.as_ref() {
            validate_outcome_coordinates(outcome, &status)?;
        }
        Ok(Self {
            status,
            quarantine_origin,
            latest_transfer_outcome,
            retired_enrollment,
        })
    }

    pub(crate) fn status(&self) -> &HostGovernanceStatus {
        &self.status
    }

    pub(crate) fn latest_transfer_outcome(&self) -> Option<&SignedTransferOutcome> {
        self.latest_transfer_outcome.as_ref()
    }

    pub(crate) const fn quarantine_origin(&self) -> Option<EnrollmentStatus> {
        self.quarantine_origin
    }

    pub(crate) fn retired_enrollment(&self) -> Option<&RetiredEnrollment> {
        self.retired_enrollment.as_ref()
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, HostStateRepositoryError> {
        let bytes = serde_json::to_vec(&GovernanceWire {
            format_version: GOVERNANCE_FORMAT_VERSION,
            host_id: self.status.host_id(),
            approval_key_reference: self.status.approval_key_reference(),
            enrollment: self.status.enrollment(),
            owner: self.status.owner(),
            owner_revision: self.status.owner_revision(),
            quarantine: self.status.quarantine(),
            quarantine_origin: self.quarantine_origin,
            latest_transfer_outcome: self.latest_transfer_outcome.as_ref(),
            retired_enrollment: self.retired_enrollment.as_ref(),
        })
        .map_err(|_error| HostStateRepositoryError::InvalidRecord {
            record: "governance.json",
        })?;
        if bytes.len() > MAX_GOVERNANCE_RECORD_BYTES {
            return Err(HostStateRepositoryError::RecordTooLarge {
                record: "governance.json",
                max_bytes: MAX_GOVERNANCE_RECORD_BYTES,
            });
        }
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, HostStateRepositoryError> {
        if bytes.len() > MAX_GOVERNANCE_RECORD_BYTES {
            return Err(HostStateRepositoryError::RecordTooLarge {
                record: "governance.json",
                max_bytes: MAX_GOVERNANCE_RECORD_BYTES,
            });
        }
        let wire: GovernanceWireOwned = serde_json::from_slice(bytes).map_err(|_error| {
            HostStateRepositoryError::InvalidRecord {
                record: "governance.json",
            }
        })?;
        if wire.format_version != GOVERNANCE_FORMAT_VERSION {
            return Err(HostStateRepositoryError::UnsupportedRecordVersion {
                record: "governance.json",
            });
        }
        let status = HostGovernanceStatus::new(
            wire.host_id,
            wire.enrollment,
            wire.owner,
            wire.owner_revision,
            wire.quarantine,
            wire.approval_key_reference,
        )
        .map_err(|_error| HostStateRepositoryError::InvalidRecord {
            record: "governance.json",
        })?;
        Self::with_quarantine_origin(
            status,
            wire.quarantine_origin,
            wire.latest_transfer_outcome,
            wire.retired_enrollment,
        )
    }
}

fn retired_status(
    current: &HostGovernanceStatus,
    retired: &RetiredEnrollment,
) -> Result<HostGovernanceStatus, HostStateRepositoryError> {
    HostGovernanceStatus::new(
        current.host_id().clone(),
        Some(retired.enrollment().clone()),
        Some(retired.owner().clone()),
        Some(retired.owner_revision()),
        None,
        current.approval_key_reference().clone(),
    )
    .map_err(|_error| HostStateRepositoryError::InvalidRecord {
        record: "governance.json",
    })
}

fn validate_outcome_coordinates(
    outcome: &SignedTransferOutcome,
    status: &HostGovernanceStatus,
) -> Result<(), HostStateRepositoryError> {
    let candidate = outcome.candidate();
    let proposal = candidate.proposal();
    if proposal.host_id() != status.host_id()
        || proposal.relay_id()
            != status
                .enrollment()
                .ok_or(HostStateRepositoryError::InvalidRecord {
                    record: "governance.json",
                })?
                .relay_id()
        || candidate.approval_key_reference() != status.approval_key_reference()
        || Some(candidate.new_owner_revision()) != status.owner_revision()
        || status.owner() != Some(proposal.target())
        || candidate.new_owner_revision()
            != proposal.owner_revision().checked_next().map_err(|_error| {
                HostStateRepositoryError::InvalidRecord {
                    record: "governance.json",
                }
            })?
    {
        return Err(HostStateRepositoryError::InvalidRecord {
            record: "governance.json",
        });
    }
    Ok(())
}

#[derive(Serialize)]
struct GovernanceWire<'a> {
    format_version: u8,
    host_id: &'a HostId,
    approval_key_reference: &'a ApprovalKeyReference,
    enrollment: Option<&'a EnrollmentInfo>,
    owner: Option<&'a HostOwner>,
    owner_revision: Option<OwnerRevision>,
    quarantine: Option<QuarantineReason>,
    quarantine_origin: Option<EnrollmentStatus>,
    latest_transfer_outcome: Option<&'a SignedTransferOutcome>,
    retired_enrollment: Option<&'a RetiredEnrollment>,
}

struct GovernanceWireOwned {
    format_version: u8,
    host_id: HostId,
    approval_key_reference: ApprovalKeyReference,
    enrollment: Option<EnrollmentInfo>,
    owner: Option<HostOwner>,
    owner_revision: Option<OwnerRevision>,
    quarantine: Option<QuarantineReason>,
    quarantine_origin: Option<EnrollmentStatus>,
    latest_transfer_outcome: Option<SignedTransferOutcome>,
    retired_enrollment: Option<RetiredEnrollment>,
}

impl<'de> Deserialize<'de> for GovernanceWireOwned {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct GovernanceVisitor;

        impl<'de> Visitor<'de> for GovernanceVisitor {
            type Value = GovernanceWireOwned;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a governance record object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut format_version: Option<u8> = None;
                let mut host_id: Option<HostId> = None;
                let mut approval_key_reference: Option<ApprovalKeyReference> = None;
                let mut enrollment: Option<Option<EnrollmentInfo>> = None;
                let mut owner: Option<Option<HostOwner>> = None;
                let mut owner_revision: Option<Option<OwnerRevision>> = None;
                let mut quarantine: Option<Option<QuarantineReason>> = None;
                let mut quarantine_origin: Option<Option<EnrollmentStatus>> = None;
                let mut latest_transfer_outcome: Option<Option<SignedTransferOutcome>> = None;
                let mut retired_enrollment: Option<Option<RetiredEnrollment>> = None;
                while let Some(field) = map.next_key::<String>()? {
                    match field.as_str() {
                        "format_version" => {
                            set_once(&mut format_version, map.next_value()?, "format_version")?;
                        }
                        "host_id" => set_once(&mut host_id, map.next_value()?, "host_id")?,
                        "approval_key_reference" => set_once(
                            &mut approval_key_reference,
                            map.next_value()?,
                            "approval_key_reference",
                        )?,
                        "enrollment" => set_once(&mut enrollment, map.next_value()?, "enrollment")?,
                        "owner" => set_once(&mut owner, map.next_value()?, "owner")?,
                        "owner_revision" => {
                            set_once(&mut owner_revision, map.next_value()?, "owner_revision")?;
                        }
                        "quarantine" => set_once(&mut quarantine, map.next_value()?, "quarantine")?,
                        "quarantine_origin" => set_once(
                            &mut quarantine_origin,
                            map.next_value()?,
                            "quarantine_origin",
                        )?,
                        "latest_transfer_outcome" => set_once(
                            &mut latest_transfer_outcome,
                            map.next_value()?,
                            "latest_transfer_outcome",
                        )?,
                        "retired_enrollment" => set_once(
                            &mut retired_enrollment,
                            map.next_value()?,
                            "retired_enrollment",
                        )?,
                        _ => {
                            return Err(serde::de::Error::unknown_field(&field, GOVERNANCE_FIELDS))
                        }
                    }
                }
                Ok(GovernanceWireOwned {
                    format_version: required(format_version, "format_version")?,
                    host_id: required(host_id, "host_id")?,
                    approval_key_reference: required(
                        approval_key_reference,
                        "approval_key_reference",
                    )?,
                    enrollment: required(enrollment, "enrollment")?,
                    owner: required(owner, "owner")?,
                    owner_revision: required(owner_revision, "owner_revision")?,
                    quarantine: required(quarantine, "quarantine")?,
                    quarantine_origin: required(quarantine_origin, "quarantine_origin")?,
                    latest_transfer_outcome: required(
                        latest_transfer_outcome,
                        "latest_transfer_outcome",
                    )?,
                    retired_enrollment: required(retired_enrollment, "retired_enrollment")?,
                })
            }
        }

        deserializer.deserialize_struct("GovernanceRecord", GOVERNANCE_FIELDS, GovernanceVisitor)
    }
}

const GOVERNANCE_FIELDS: &[&str] = &[
    "format_version",
    "host_id",
    "approval_key_reference",
    "enrollment",
    "owner",
    "owner_revision",
    "quarantine",
    "quarantine_origin",
    "latest_transfer_outcome",
    "retired_enrollment",
];

fn set_once<E: serde::de::Error, T>(
    target: &mut Option<T>,
    value: T,
    field: &'static str,
) -> Result<(), E> {
    if target.replace(value).is_some() {
        Err(E::duplicate_field(field))
    } else {
        Ok(())
    }
}

fn required<E: serde::de::Error, T>(value: Option<T>, field: &'static str) -> Result<T, E> {
    value.ok_or_else(|| E::missing_field(field))
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    const HOST_ID: &str = "host_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const RELAY_ID: &str = "relay_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const PRINCIPAL_ID: &str = "principal_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn host_id() -> HostId {
        HostId::parse(HOST_ID).expect("valid fixed host identity")
    }

    fn key_reference() -> ApprovalKeyReference {
        ApprovalKeyReference::from_ed25519_verifying_key_bytes([7; 32])
    }

    fn current_governance() -> GovernanceState {
        let status = HostGovernanceStatus::new(
            host_id(),
            Some(EnrollmentInfo::new(
                protocol::RelayId::parse(RELAY_ID).expect("valid relay"),
                EnrollmentStatus::Active,
                protocol::EnrollmentRevision::new(1).expect("valid enrollment revision"),
            )),
            Some(HostOwner::Principal(
                protocol::PrincipalId::parse(PRINCIPAL_ID).expect("valid principal"),
            )),
            Some(OwnerRevision::new(1).expect("valid owner revision")),
            None,
            key_reference(),
        )
        .expect("valid current governance");
        GovernanceState::new(status, None, None).expect("valid governance state")
    }

    fn quarantined_governance(origin: EnrollmentStatus) -> GovernanceState {
        let status = HostGovernanceStatus::new(
            host_id(),
            Some(EnrollmentInfo::new(
                protocol::RelayId::parse(RELAY_ID).expect("valid relay"),
                EnrollmentStatus::Quarantined,
                protocol::EnrollmentRevision::new(2).expect("valid enrollment revision"),
            )),
            Some(HostOwner::Principal(
                protocol::PrincipalId::parse(PRINCIPAL_ID).expect("valid principal"),
            )),
            Some(OwnerRevision::new(1).expect("valid owner revision")),
            Some(QuarantineReason::ProjectionConflict),
            key_reference(),
        )
        .expect("valid quarantined governance");
        GovernanceState::with_quarantine_origin(status, Some(origin), None, None)
            .expect("valid quarantined provenance")
    }

    fn retired_governance() -> GovernanceState {
        let retired = RetiredEnrollment::new(
            EnrollmentInfo::new(
                protocol::RelayId::parse(RELAY_ID).expect("valid relay"),
                EnrollmentStatus::LocallyUnenrolled,
                protocol::EnrollmentRevision::new(1).expect("valid enrollment revision"),
            ),
            HostOwner::Principal(
                protocol::PrincipalId::parse(PRINCIPAL_ID).expect("valid principal"),
            ),
            OwnerRevision::new(1).expect("valid owner revision"),
            None,
        )
        .expect("valid retired summary");
        GovernanceState::new(
            HostGovernanceStatus::new(
                host_id(),
                Some(EnrollmentInfo::new(
                    protocol::RelayId::parse(RELAY_ID).expect("valid relay"),
                    EnrollmentStatus::PendingLocalCommit,
                    protocol::EnrollmentRevision::new(2)
                        .expect("valid current enrollment revision"),
                )),
                Some(HostOwner::Principal(
                    protocol::PrincipalId::parse(PRINCIPAL_ID).expect("valid principal"),
                )),
                Some(OwnerRevision::new(2).expect("valid current owner revision")),
                None,
                key_reference(),
            )
            .expect("valid current status following a retired enrollment"),
            None,
            Some(retired),
        )
        .expect("valid retired governance")
    }

    fn assert_invalid(bytes: &[u8]) {
        assert!(matches!(
            GovernanceState::decode(bytes),
            Err(HostStateRepositoryError::InvalidRecord {
                record: "governance.json"
            })
        ));
    }

    fn encoded_value(state: &GovernanceState) -> Value {
        serde_json::from_slice(&state.encode().expect("encode governance"))
            .expect("governance is JSON")
    }

    #[test]
    fn governance_top_level_rejects_duplicate_unknown_missing_and_future_versions() {
        let encoded = current_governance().encode().expect("encode governance");
        let encoded = String::from_utf8(encoded).expect("governance is UTF-8 JSON");
        let duplicate = format!(
            "{},\"format_version\":2}}",
            encoded.strip_suffix('}').expect("JSON object")
        );
        assert_invalid(duplicate.as_bytes());

        let mut unknown = encoded_value(&current_governance());
        unknown["unexpected"] = Value::Null;
        assert_invalid(&serde_json::to_vec(&unknown).expect("encode unknown field"));

        let mut missing = encoded_value(&current_governance());
        missing
            .as_object_mut()
            .expect("top-level object")
            .remove("approval_key_reference");
        assert_invalid(&serde_json::to_vec(&missing).expect("encode missing field"));

        let mut future = encoded_value(&current_governance());
        future["format_version"] = Value::from(3);
        assert!(matches!(
            GovernanceState::decode(&serde_json::to_vec(&future).expect("encode future version")),
            Err(HostStateRepositoryError::UnsupportedRecordVersion {
                record: "governance.json"
            })
        ));

        let mut old = encoded_value(&current_governance());
        old["format_version"] = Value::from(1);
        assert!(matches!(
            GovernanceState::decode(&serde_json::to_vec(&old).expect("encode old version")),
            Err(HostStateRepositoryError::UnsupportedRecordVersion {
                record: "governance.json"
            })
        ));
    }

    #[test]
    fn quarantine_origin_is_required_private_and_lifecycle_consistent() {
        let current = current_governance();
        let mut missing = encoded_value(&current);
        missing
            .as_object_mut()
            .expect("governance object")
            .remove("quarantine_origin");
        assert_invalid(&serde_json::to_vec(&missing).expect("encode missing origin"));

        let current_encoded = String::from_utf8(current.encode().expect("encode governance"))
            .expect("governance is UTF-8 JSON");
        let duplicate = format!(
            "{},\"quarantine_origin\":null}}",
            current_encoded.strip_suffix('}').expect("JSON object")
        );
        assert_invalid(duplicate.as_bytes());

        let mut active_with_origin = encoded_value(&current);
        active_with_origin["quarantine_origin"] = Value::from("active");
        assert_invalid(&serde_json::to_vec(&active_with_origin).expect("encode active provenance"));

        let quarantined = quarantined_governance(EnrollmentStatus::Active);
        let decoded = GovernanceState::decode(&quarantined.encode().expect("encode quarantine"))
            .expect("decode quarantined governance");
        assert_eq!(decoded.quarantine_origin(), Some(EnrollmentStatus::Active));

        let mut missing_origin = encoded_value(&quarantined);
        missing_origin["quarantine_origin"] = Value::Null;
        assert_invalid(
            &serde_json::to_vec(&missing_origin).expect("encode missing quarantined origin"),
        );

        let mut quarantined_origin = encoded_value(&quarantined);
        quarantined_origin["quarantine_origin"] = Value::from("quarantined");
        assert_invalid(&serde_json::to_vec(&quarantined_origin).expect("encode recursive origin"));

        let mut unknown_origin = encoded_value(&quarantined);
        unknown_origin["quarantine_origin"] = Value::from("unknown");
        assert_invalid(&serde_json::to_vec(&unknown_origin).expect("encode unknown origin"));
    }

    #[test]
    fn governance_nested_current_and_retired_objects_reject_duplicate_unknown_and_missing_fields() {
        let current = current_governance();
        let current_encoded = String::from_utf8(current.encode().expect("encode governance"))
            .expect("governance is UTF-8 JSON");
        let duplicate_current = current_encoded.replacen(
            "\"enrollment\":{",
            &format!("\"enrollment\":{{\"relay_id\":\"{RELAY_ID}\","),
            1,
        );
        assert_invalid(duplicate_current.as_bytes());

        let mut unknown_current = encoded_value(&current);
        unknown_current["enrollment"]["unexpected"] = Value::Null;
        assert_invalid(&serde_json::to_vec(&unknown_current).expect("encode unknown enrollment"));
        let mut missing_current = encoded_value(&current);
        missing_current["enrollment"]
            .as_object_mut()
            .expect("enrollment object")
            .remove("status");
        assert_invalid(&serde_json::to_vec(&missing_current).expect("encode missing enrollment"));

        let retired = retired_governance();
        let retired_encoded = String::from_utf8(retired.encode().expect("encode governance"))
            .expect("governance is UTF-8 JSON");
        let duplicate_retired = retired_encoded.replacen(
            "\"retired_enrollment\":{",
            "\"retired_enrollment\":{\"owner_revision\":\"1\",",
            1,
        );
        assert_invalid(duplicate_retired.as_bytes());

        let mut unknown_retired = encoded_value(&retired);
        unknown_retired["retired_enrollment"]["unexpected"] = Value::Null;
        assert_invalid(
            &serde_json::to_vec(&unknown_retired).expect("encode unknown retired summary"),
        );
        let mut missing_retired = encoded_value(&retired);
        missing_retired["retired_enrollment"]
            .as_object_mut()
            .expect("retired summary object")
            .remove("owner");
        assert_invalid(
            &serde_json::to_vec(&missing_retired).expect("encode missing retired summary"),
        );
    }
}
