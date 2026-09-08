//! Account and credential lifecycle contracts for authenticated relay clients.

use crate::{CredentialId, DeviceCredential, Idempotency, PrincipalId, TeamId};
use serde::{Deserialize, Serialize};
use std::fmt::{Debug, Formatter};
use time::OffsetDateTime;
use uuid::Uuid;

/// The durable principal category.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    Human,
    Service,
    Infrastructure,
}

/// Current account lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(rename_all = "snake_case")]
pub enum PrincipalState {
    Active,
    Deprovisioned,
}

/// Native human and service credentials are distinct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    Human,
    Service,
}

/// A stable identity explicitly linked to this account.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct IdentityRecord {
    pub identity_id: Uuid,
    pub issuer: String,
    pub subject: String,
}

impl Debug for IdentityRecord {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdentityRecord")
            .field("identity_id", &self.identity_id)
            .finish_non_exhaustive()
    }
}

/// The authenticated account; identities confer no team grants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct AccountRecord {
    pub principal_id: PrincipalId,
    pub kind: PrincipalKind,
    pub state: PrincipalState,
    pub identities: Vec<IdentityRecord>,
}

/// Non-secret metadata for a credential owned by the selected account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct CredentialRecord {
    pub credential_id: CredentialId,
    pub principal_id: PrincipalId,
    pub kind: CredentialKind,
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "time::serde::rfc3339")]
    pub issued_at: OffsetDateTime,
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    #[cfg_attr(feature = "ts", ts(type = "string | null"))]
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub rotation_overlap_ends_at: Option<OffsetDateTime>,
    #[cfg_attr(feature = "ts", ts(type = "string | null"))]
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub last_used_at: Option<OffsetDateTime>,
    #[cfg_attr(feature = "ts", ts(type = "string | null"))]
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub revoked_at: Option<OffsetDateTime>,
}

/// A bounded cursor page of credential metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct CredentialPage {
    pub records: Vec<CredentialRecord>,
    pub next_cursor: Option<Uuid>,
}

/// Team-scoped service principal metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct ServiceAccountRecord {
    pub principal_id: PrincipalId,
    pub team_id: TeamId,
    pub display_name: String,
    pub state: PrincipalState,
}

/// A bounded cursor page of team service accounts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct ServiceAccountPage {
    pub records: Vec<ServiceAccountRecord>,
    pub next_cursor: Option<Uuid>,
}

/// Creates a service account without implicit permission grants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct CreateServiceAccountRequest {
    pub display_name: String,
    pub idempotency: Idempotency,
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

/// Issues a replacement with an explicitly bounded overlap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct RotateCredentialRequest {
    pub overlap_seconds: u32,
    pub idempotency: Idempotency,
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

/// Revokes a credential with an exact caller-provided retry coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct RevokeCredentialRequest {
    pub idempotency: Idempotency,
}

/// Starts explicit proof of a second identity for the current human account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct AccountLinkRequest {
    pub idempotency: Idempotency,
}

/// Delivers a credential once; an exact retry returns metadata with no secret.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct CredentialMutation {
    pub record: CredentialRecord,
    pub credential: Option<DeviceCredential>,
}

/// Creates the service principal and delivers its first credential once.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct ServiceAccountCreated {
    pub account: ServiceAccountRecord,
    pub issued: CredentialMutation,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_identity_debug_omits_stable_subjects() {
        let identity = IdentityRecord {
            identity_id: Uuid::nil(),
            issuer: "https://issuer.example".to_owned(),
            subject: "sentinel-subject".to_owned(),
        };
        let account = AccountRecord {
            principal_id: PrincipalId::from_uuid(Uuid::nil()),
            kind: PrincipalKind::Human,
            state: PrincipalState::Active,
            identities: vec![identity],
        };
        assert!(!format!("{account:?}").contains("sentinel-subject"));
        let wire = serde_json::to_value(&account).expect("account JSON");
        assert_eq!(
            serde_json::from_value::<AccountRecord>(wire).expect("account round trip"),
            account
        );
    }

    #[test]
    fn credential_metadata_uses_rfc3339_and_explicit_nulls() {
        let credential = CredentialRecord {
            credential_id: CredentialId::from_uuid(Uuid::nil()),
            principal_id: PrincipalId::from_uuid(Uuid::nil()),
            kind: CredentialKind::Human,
            issued_at: OffsetDateTime::UNIX_EPOCH,
            expires_at: OffsetDateTime::UNIX_EPOCH,
            rotation_overlap_ends_at: None,
            last_used_at: None,
            revoked_at: None,
        };
        let wire = serde_json::to_value(&credential).expect("credential JSON");
        assert_eq!(wire["issued_at"], "1970-01-01T00:00:00Z");
        assert!(wire["last_used_at"].is_null());
        assert_eq!(
            serde_json::from_value::<CredentialRecord>(wire).expect("credential round trip"),
            credential
        );
    }
}
