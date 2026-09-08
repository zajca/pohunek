//! Untrusted broker evidence transport contract.
//!
//! Deserialization proves only the wire shape. These values never constitute an
//! authorization decision; the future broker verifier must authenticate the
//! transport, signature, one-use challenge, audience, generations, and freshness.

use std::fmt::{Debug, Formatter};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{PrincipalId, RelayId, Secret, TeamId};

/// Version of the narrow broker evidence boundary defined by the relay RFC.
pub const EVIDENCE_SCHEMA_VERSION: u16 = 1;

/// Exact supported version; unknown schemas cannot deserialize as version one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, type = "1"))]
pub struct EvidenceVersion;

impl Serialize for EvidenceVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u16(EVIDENCE_SCHEMA_VERSION)
    }
}

impl<'de> Deserialize<'de> for EvidenceVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if u16::deserialize(deserializer)? != EVIDENCE_SCHEMA_VERSION {
            return Err(serde::de::Error::custom(
                "unsupported evidence schema version",
            ));
        }
        Ok(Self)
    }
}

/// Authoritative upstream identity source, distinct from the broker issuer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(rename_all = "snake_case")]
pub enum EvidenceProvider {
    Google,
    #[serde(rename = "github")]
    GitHub,
}

/// Authoritative provider check represented by an attestation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(rename_all = "snake_case")]
pub enum EvidenceMethod {
    GoogleHostedDomain,
    #[serde(rename = "github_organization")]
    GitHubOrganization,
}

/// Only a separately verified eligible result can contribute to authorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(rename_all = "snake_case")]
pub enum EvidenceOutcome {
    Eligible,
    Ineligible,
    Unknown,
}

/// Audience and one-use transaction coordinates returned by the broker.
#[derive(PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct EvidenceClaims {
    pub evidence_version: EvidenceVersion,
    pub evidence_id: Uuid,
    pub challenge_id: Uuid,
    pub transaction_id: Uuid,
    pub nonce: Secret,
    pub audience: RelayId,
    pub principal_id: PrincipalId,
    pub issuer: String,
    pub keycloak_subject: String,
    pub provider: EvidenceProvider,
    pub provider_subject: String,
    pub team_id: TeamId,
    pub admission_rule_id: Uuid,
    #[serde(with = "crate::revision")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub admission_rule_revision: i64,
    #[serde(with = "crate::revision")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub account_link_generation: i64,
    #[serde(with = "crate::revision")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub provider_identity_generation: i64,
    #[serde(with = "crate::revision")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub recovery_generation: i64,
    #[serde(with = "time::serde::rfc3339")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub checked_at_utc: OffsetDateTime,
    pub checked_monotonic_epoch: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub expires_at_utc: OffsetDateTime,
    pub method: EvidenceMethod,
    pub outcome: EvidenceOutcome,
    pub binding_transaction_digest: String,
    pub upstream_exchange_digest: String,
}

impl Debug for EvidenceClaims {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvidenceClaims")
            .field("evidence_version", &self.evidence_version)
            .field("evidence_id", &self.evidence_id)
            .field("audience", &self.audience)
            .field("team_id", &self.team_id)
            .field("outcome", &self.outcome)
            .finish_non_exhaustive()
    }
}

/// Unverified signature over canonical JSON claims, identified by the pinned key.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct SignedEvidence {
    pub signing_key_id: String,
    pub claims: EvidenceClaims,
    pub signature: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims() -> serde_json::Value {
        serde_json::json!({
            "evidence_version": 1, "evidence_id": Uuid::nil(), "challenge_id": Uuid::nil(),
            "transaction_id": Uuid::nil(), "nonce": "sentinel-nonce",
            "audience": format!("relay_{}", "A".repeat(43)), "principal_id": Uuid::nil(),
            "issuer": "https://issuer.example/realms/test", "keycloak_subject": "sentinel-broker-subject",
            "provider": "github", "provider_subject": "sentinel-provider-subject", "team_id": Uuid::nil(),
            "admission_rule_id": Uuid::nil(), "admission_rule_revision": "9007199254740993",
            "account_link_generation": "1", "provider_identity_generation": "1", "recovery_generation": "1",
            "checked_at_utc": "2026-09-08T00:00:00Z", "checked_monotonic_epoch": Uuid::nil(),
            "expires_at_utc": "2026-09-08T01:00:00Z", "method": "github_organization", "outcome": "eligible",
            "binding_transaction_digest": "sentinel-binding", "upstream_exchange_digest": "sentinel-upstream"
        })
    }

    #[test]
    fn evidence_round_trip_preserves_binding_coordinates_without_debugging_subjects() {
        let wire = claims();
        let claims: EvidenceClaims =
            serde_json::from_value(wire.clone()).expect("version one shape");
        assert!(!format!("{claims:?}").contains("sentinel"));
        assert_eq!(serde_json::to_value(claims).expect("evidence JSON"), wire);
    }

    #[test]
    fn evidence_rejects_future_versions_missing_bindings_and_self_selected_fields() {
        for version in [
            serde_json::json!(2),
            serde_json::json!("1"),
            serde_json::json!(0),
        ] {
            let mut wire = claims();
            wire["evidence_version"] = version;
            serde_json::from_value::<EvidenceClaims>(wire).expect_err("unknown version");
        }
        for field in [
            "audience",
            "challenge_id",
            "nonce",
            "team_id",
            "admission_rule_revision",
            "provider_subject",
            "expires_at_utc",
        ] {
            let mut wire = claims();
            wire.as_object_mut().expect("object").remove(field);
            serde_json::from_value::<EvidenceClaims>(wire).expect_err("missing binding");
        }
        let mut wire = claims();
        wire["email_verified"] = serde_json::json!(true);
        serde_json::from_value::<EvidenceClaims>(wire)
            .expect_err("unrecognized claim cannot enter contract");
    }
}
