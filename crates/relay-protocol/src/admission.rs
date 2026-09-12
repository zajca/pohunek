//! Versioned social admission rule and evidence result contracts.
//!
//! Deserialization proves only the wire shape. These values never authorize a
//! join; the future verifier and membership transaction own that decision.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{EvidenceMethod, EvidenceOutcome, EvidenceProvider, Idempotency, TeamId};

/// Upstream identity source bound by an admission rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(rename_all = "snake_case")]
pub enum AdmissionProvider {
    Google,
    #[serde(rename = "github")]
    GitHub,
}

/// Authoritative provider check bound by an admission rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(rename_all = "snake_case")]
pub enum AdmissionMethod {
    GoogleHostedDomain,
    #[serde(rename = "github_organization")]
    GitHubOrganization,
}

/// Lifecycle state of a versioned admission rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(rename_all = "snake_case")]
pub enum AdmissionRuleState {
    Active,
    Disabled,
}

/// Durable versioned admission rule coordinate for one team.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct AdmissionRuleRecord {
    pub admission_rule_id: Uuid,
    pub team_id: TeamId,
    pub provider: AdmissionProvider,
    pub method: AdmissionMethod,
    pub match_value: String,
    #[serde(with = "crate::revision")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub revision: i64,
    pub state: AdmissionRuleState,
}

/// Creates one admission rule with an explicit retry coordinate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct CreateAdmissionRuleRequest {
    pub provider: AdmissionProvider,
    pub method: AdmissionMethod,
    pub match_value: String,
    pub idempotency: Idempotency,
}

/// Updates one admission rule against an exact optimistic revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct UpdateAdmissionRuleRequest {
    pub admission_rule_id: Uuid,
    #[serde(with = "crate::revision")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub expected_revision: i64,
    pub disable: bool,
    pub idempotency: Idempotency,
}

/// Committed broker evidence result bound to one consumed challenge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct EvidenceResultRecord {
    pub evidence_id: Uuid,
    pub challenge_id: Uuid,
    pub team_id: TeamId,
    pub admission_rule_id: Uuid,
    #[serde(with = "crate::revision")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub admission_rule_revision: i64,
    pub provider: EvidenceProvider,
    pub method: EvidenceMethod,
    pub outcome: EvidenceOutcome,
    #[serde(with = "time::serde::rfc3339")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub checked_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub expires_at: OffsetDateTime,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> AdmissionRuleRecord {
        AdmissionRuleRecord {
            admission_rule_id: Uuid::nil(),
            team_id: TeamId::from_uuid(Uuid::nil()),
            provider: AdmissionProvider::GitHub,
            method: AdmissionMethod::GitHubOrganization,
            match_value: "example-org".to_owned(),
            revision: 1,
            state: AdmissionRuleState::Active,
        }
    }

    fn result() -> EvidenceResultRecord {
        EvidenceResultRecord {
            evidence_id: Uuid::nil(),
            challenge_id: Uuid::nil(),
            team_id: TeamId::from_uuid(Uuid::nil()),
            admission_rule_id: Uuid::nil(),
            admission_rule_revision: 1,
            provider: EvidenceProvider::GitHub,
            method: EvidenceMethod::GitHubOrganization,
            outcome: EvidenceOutcome::Eligible,
            checked_at: OffsetDateTime::UNIX_EPOCH,
            expires_at: OffsetDateTime::UNIX_EPOCH + time::Duration::minutes(30),
        }
    }

    #[test]
    fn admission_round_trip_preserves_rule_and_result() {
        let rule_wire = serde_json::to_value(rule()).expect("rule JSON");
        assert_eq!(
            serde_json::from_value::<AdmissionRuleRecord>(rule_wire).expect("rule"),
            rule()
        );
        let result_wire = serde_json::to_value(result()).expect("result JSON");
        assert_eq!(
            serde_json::from_value::<EvidenceResultRecord>(result_wire).expect("result"),
            result()
        );
        let create = CreateAdmissionRuleRequest {
            provider: AdmissionProvider::Google,
            method: AdmissionMethod::GoogleHostedDomain,
            match_value: "example.com".to_owned(),
            idempotency: Idempotency {
                correlation_id: Uuid::nil(),
                idempotency_key: Uuid::nil(),
            },
        };
        let create_wire = serde_json::to_value(&create).expect("create JSON");
        assert_eq!(
            serde_json::from_value::<CreateAdmissionRuleRequest>(create_wire).expect("create"),
            create
        );
    }

    #[test]
    fn admission_rejects_unknown_subject_fields() {
        let mut rule_wire = serde_json::to_value(rule()).expect("rule JSON");
        rule_wire["provider_subject"] = serde_json::json!("sentinel");
        serde_json::from_value::<AdmissionRuleRecord>(rule_wire).expect_err("subject");
        let mut result_wire = serde_json::to_value(result()).expect("result JSON");
        result_wire["provider_subject"] = serde_json::json!("sentinel");
        serde_json::from_value::<EvidenceResultRecord>(result_wire).expect_err("subject");
    }

    #[test]
    fn evidence_result_deadlines_use_rfc3339_strings() {
        let wire = serde_json::to_value(result()).expect("result JSON");
        assert_eq!(wire["checked_at"], "1970-01-01T00:00:00Z");
        assert_eq!(wire["expires_at"], "1970-01-01T00:30:00Z");
        assert_eq!(wire["admission_rule_revision"], "1");
        let mut bad = serde_json::to_value(result()).expect("result JSON");
        bad["checked_at"] = serde_json::json!(0);
        serde_json::from_value::<EvidenceResultRecord>(bad).expect_err("numeric time");
    }
}
