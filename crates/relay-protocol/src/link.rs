//! Account-linking contracts for proving a second stable OIDC identity.
//!
//! Identity is always the exact issuer plus immutable subject. No type here
//! carries an email, display name, or any other provider profile attribute,
//! because those are never linking inputs.

use crate::{Idempotency, IdentityRecord, PrincipalId, Secret};
use serde::{Deserialize, Serialize};
use std::fmt::{Debug, Formatter};
use time::OffsetDateTime;
use uuid::Uuid;

/// Possession channel that must complete a link transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(rename_all = "snake_case")]
pub enum AccountLinkChannel {
    /// Completed by the same browser session through the registered callback.
    Browser,
    /// Completed by the same bearer credential through device authorization.
    Device,
}

/// Current disposition of one account-link transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(rename_all = "snake_case")]
pub enum AccountLinkState {
    /// Still open and completable by its bound possession material.
    Pending,
    /// The new identity was proven and linked.
    Completed,
    /// Explicitly cancelled by the account or by recovery quarantine.
    Cancelled,
    /// Passed its bounded expiry without proof.
    Expired,
    /// Terminally rejected by the relay or the provider.
    Failed,
}

/// Safe revisioned view of one account-link transaction.
///
/// Every field is a durable coordinate; possession digests, provider tokens,
/// and verifier material never appear in this record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct AccountLinkRecord {
    pub link_id: Uuid,
    pub principal_id: PrincipalId,
    pub channel: AccountLinkChannel,
    pub state: AccountLinkState,
    pub source_identity_id: Uuid,
    pub linked_identity_id: Option<Uuid>,
    /// Advances on every durable transition of this transaction.
    pub revision: i64,
    /// Account generation this transaction was bound to when it was created.
    pub account_link_generation: i64,
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    #[cfg_attr(feature = "ts", ts(type = "string | null"))]
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub completed_at: Option<OffsetDateTime>,
}

/// A bounded cursor page of the account's link transactions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct AccountLinkPage {
    pub records: Vec<AccountLinkRecord>,
    pub next_cursor: Option<Uuid>,
}

/// Browser link start; the caller navigates to the returned provider URL.
///
/// The response also sets the one-use host-only binding cookie that the
/// callback must return, so the URL alone cannot complete the transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct AccountLinkBrowserStart {
    pub link: AccountLinkRecord,
    pub authorization_url: String,
}

/// RFC 8628 values and the one-use poll secret for one link transaction.
///
/// The transaction is polled by its `link_id`, so this carries no separate
/// login coordinate.
#[derive(PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct LinkDeviceAuthorization {
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub user_code: String,
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    pub interval_seconds: u32,
    pub poll_secret: Secret,
}

impl Debug for LinkDeviceAuthorization {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkDeviceAuthorization")
            .field("expires_at", &self.expires_at)
            .field("interval_seconds", &self.interval_seconds)
            .finish_non_exhaustive()
    }
}

/// Device link start delivering RFC 8628 values and the one-use poll secret.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct AccountLinkDeviceStart {
    pub link: AccountLinkRecord,
    pub authorization: LinkDeviceAuthorization,
}

/// Polls a device link without putting its possession secret in a URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct AccountLinkPollRequest {
    pub link_id: Uuid,
}

/// Safe device link poll response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum AccountLinkPollResult {
    Pending { retry_after_seconds: u32 },
    SlowDown { retry_after_seconds: u32 },
    Complete { link: AccountLinkRecord, identity: IdentityRecord },
    Denied,
    Expired,
    Cancelled,
}

/// Cancels one pending link transaction with an exact retry coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct CancelAccountLinkRequest {
    pub idempotency: Idempotency,
}

/// Removes one linked identity with an exact retry coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct UnlinkIdentityRequest {
    pub idempotency: Idempotency,
}

/// Result of removing one linked identity from the account.
///
/// The advanced account generation is the coordinate that invalidates every
/// credential, browser session, and in-flight link transaction derived from the
/// removed identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct IdentityRemoved {
    pub identity_id: Uuid,
    pub principal_id: PrincipalId,
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "time::serde::rfc3339")]
    pub removed_at: OffsetDateTime,
    pub account_link_generation: i64,
    pub revoked_credentials: u32,
    pub revoked_sessions: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> AccountLinkRecord {
        AccountLinkRecord {
            link_id: Uuid::nil(),
            principal_id: PrincipalId::from_uuid(Uuid::nil()),
            channel: AccountLinkChannel::Browser,
            state: AccountLinkState::Pending,
            source_identity_id: Uuid::nil(),
            linked_identity_id: None,
            revision: 1,
            account_link_generation: 1,
            created_at: OffsetDateTime::UNIX_EPOCH,
            expires_at: OffsetDateTime::UNIX_EPOCH,
            completed_at: None,
        }
    }

    #[test]
    fn device_link_start_redacts_its_verification_and_poll_secrets() {
        let start = AccountLinkDeviceStart {
            link: record(),
            authorization: LinkDeviceAuthorization {
                verification_uri: "https://issuer.example/device".to_owned(),
                verification_uri_complete: Some(
                    "https://issuer.example/device?user_code=sentinel-code".to_owned(),
                ),
                user_code: "sentinel-code".to_owned(),
                expires_at: OffsetDateTime::UNIX_EPOCH,
                interval_seconds: 5,
                poll_secret: Secret::new("sentinel-poll".to_owned()),
            },
        };
        let debug = format!("{start:?}");
        assert!(!debug.contains("sentinel-code"));
        assert!(!debug.contains("sentinel-poll"));
        let wire = serde_json::to_value(&start).expect("device link start JSON");
        assert_eq!(wire["authorization"]["expires_at"], "1970-01-01T00:00:00Z");
        assert_eq!(
            serde_json::from_value::<AccountLinkDeviceStart>(wire)
                .expect("device link start round trip"),
            start
        );
    }

    #[test]
    fn link_record_uses_rfc3339_and_explicit_nulls() {
        let link = record();
        let wire = serde_json::to_value(&link).expect("link JSON");
        assert_eq!(wire["created_at"], "1970-01-01T00:00:00Z");
        assert!(wire["completed_at"].is_null());
        assert!(wire["linked_identity_id"].is_null());
        assert_eq!(
            serde_json::from_value::<AccountLinkRecord>(wire).expect("link round trip"),
            link
        );
    }

    #[test]
    fn poll_states_cannot_mix_completion_and_pending() {
        let invalid = r#"{"status":"pending","link":{}}"#;
        serde_json::from_str::<AccountLinkPollResult>(invalid)
            .expect_err("pending state must reject a completed link");
    }

    #[test]
    fn completed_poll_carries_only_stable_identity_coordinates() {
        let result = AccountLinkPollResult::Complete {
            link: record(),
            identity: IdentityRecord {
                identity_id: Uuid::nil(),
                issuer: "https://issuer.example".to_owned(),
                subject: "sentinel-subject".to_owned(),
            },
        };
        assert!(!format!("{result:?}").contains("sentinel-subject"));
        let wire = serde_json::to_value(&result).expect("poll JSON");
        assert_eq!(wire["status"], "complete");
        assert_eq!(wire["identity"]["subject"], "sentinel-subject");
        assert_eq!(
            serde_json::from_value::<AccountLinkPollResult>(wire).expect("poll round trip"),
            result
        );
    }
}
