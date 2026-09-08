use std::fmt::{Debug, Formatter};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::OffsetDateTime;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::CredentialId;

/// A secret serialized only at its one-time delivery boundary.
#[derive(PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[cfg_attr(feature = "ts", ts(type = "string"))]
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// Creates a one-time secret value.
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    /// Returns the raw value for delivery to the authenticated client.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.as_str()
    }
}

/// One opaque device-login transaction identifier.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[cfg_attr(feature = "ts", ts(type = "string"))]
pub struct LoginId(String);

impl LoginId {
    /// Parses a canonical relay-local UUID login identifier.
    pub fn parse(value: &str) -> Result<Self, LoginIdError> {
        let uuid = Uuid::parse_str(value).map_err(|_error| LoginIdError::Payload)?;
        if uuid.hyphenated().to_string() != value {
            return Err(LoginIdError::Payload);
        }
        Ok(Self(value.to_owned()))
    }

    /// Creates a login identifier from the durable transaction UUID.
    #[must_use]
    pub fn from_uuid(value: Uuid) -> Self {
        Self(value.hyphenated().to_string())
    }

    /// Returns the durable transaction UUID.
    #[must_use]
    pub fn as_uuid(&self) -> Uuid {
        Uuid::parse_str(&self.0).expect("LoginId invariants require a UUID")
    }
}

impl Debug for LoginId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("LoginId").field(&self.0).finish()
    }
}

impl<'de> Deserialize<'de> for LoginId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Reports a malformed device-login identifier.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoginIdError {
    #[error("login id must be a canonical UUID")]
    Payload,
}

impl Debug for Secret {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(REDACTED)")
    }
}

impl Serialize for Secret {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.expose())
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::new)
    }
}

/// Safe response for starting OIDC device authorization.
#[derive(PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct DeviceLoginStart {
    pub login_id: LoginId,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub user_code: String,
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    pub interval_seconds: u32,
    pub poll_secret: Secret,
}

impl Debug for DeviceLoginStart {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceLoginStart")
            .field("login_id", &self.login_id)
            .field("expires_at", &self.expires_at)
            .field("interval_seconds", &self.interval_seconds)
            .finish_non_exhaustive()
    }
}

/// Safe device authorization poll response.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum DevicePollResult {
    Pending { retry_after_seconds: u32 },
    SlowDown { retry_after_seconds: u32 },
    Complete { credential: DeviceCredential },
    Denied,
    Expired,
    Cancelled,
}

/// One-time native credential delivery.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export))]
#[serde(deny_unknown_fields)]
pub struct DeviceCredential {
    pub credential_id: CredentialId,
    pub secret: Secret,
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

#[cfg(test)]
mod tests {
    use super::{DevicePollResult, Secret};

    #[test]
    fn secret_debug_is_redacted() {
        let secret = Secret::new("test-secret".to_owned());
        assert!(!format!("{secret:?}").contains(secret.expose()));
    }

    #[test]
    fn device_delivery_uses_rfc3339_and_redacts_verification_secrets() {
        let start = super::DeviceLoginStart {
            login_id: super::LoginId::from_uuid(uuid::Uuid::nil()),
            verification_uri: "https://issuer.example/device".to_owned(),
            verification_uri_complete: Some(
                "https://issuer.example/device?user_code=sentinel-code".to_owned(),
            ),
            user_code: "sentinel-code".to_owned(),
            expires_at: time::OffsetDateTime::UNIX_EPOCH,
            interval_seconds: 5,
            poll_secret: Secret::new("sentinel-poll".to_owned()),
        };
        let debug = format!("{start:?}");
        assert!(!debug.contains("sentinel-code"));
        assert!(!debug.contains("sentinel-poll"));
        let wire = serde_json::to_value(&start).expect("device start JSON");
        assert_eq!(wire["expires_at"], "1970-01-01T00:00:00Z");
        assert_eq!(
            serde_json::from_value::<super::DeviceLoginStart>(wire)
                .expect("device start round trip"),
            start
        );
        let credential = super::DeviceCredential {
            credential_id: crate::CredentialId::from_uuid(uuid::Uuid::nil()),
            secret: Secret::new("sentinel-credential".to_owned()),
            expires_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        let wire = serde_json::to_value(&credential).expect("credential JSON");
        assert_eq!(wire["expires_at"], "1970-01-01T00:00:00Z");
        assert_eq!(
            serde_json::from_value::<super::DeviceCredential>(wire).expect("credential round trip"),
            credential
        );
    }

    #[test]
    fn poll_states_cannot_mix_credentials_and_pending() {
        let invalid = r#"{"status":"pending","credential":{}}"#;
        let parsed = serde_json::from_str::<DevicePollResult>(invalid);
        parsed.expect_err("pending state must reject a credential");
    }

    #[test]
    fn login_id_rejects_invalid_wire_values() {
        for value in [
            "other_abc",
            "000000000000-0000-0000-0000-000000000000",
            "00000000-0000-0000-0000-000000000000x",
        ] {
            let parsed = serde_json::from_str::<super::LoginId>(&format!("\"{value}\""));
            parsed.expect_err("invalid login id must be rejected");
        }
    }
}
