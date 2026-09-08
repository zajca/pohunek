//! Explicit client transport settings and safe deployment ceilings.

use std::time::Duration;
use url::Url;

use crate::Error;

/// Authentication exchanges remain bounded even when the remote peer stalls.
pub const MAX_REQUEST_TIMEOUT: Duration = Duration::from_mins(1);
/// Auth/account responses are small; larger replies are rejected before decoding.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// RFC 8628 polling must not force unbounded sleeps in a native client.
pub const MAX_POLL_INTERVAL_SECONDS: u32 = 60;
/// Matches the maximum native credential lifetime permitted by relay configuration.
pub const MAX_CREDENTIAL_LIFETIME: time::Duration = time::Duration::days(90);
/// Relay device transactions cannot exceed the server's configured upper bound.
pub const MAX_LOGIN_LIFETIME: time::Duration = time::Duration::minutes(15);
/// Relay native credentials use 256 random bits encoded as canonical base64url.
pub const SECRET_BYTES: usize = 32;

/// A canonical HTTPS origin without identity, path, query, or fragment data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin(Url);

impl Origin {
    /// Validates and canonicalizes a relay origin before any request or keyring access.
    pub fn parse(value: &str) -> Result<Self, Error> {
        let url = Url::parse(value).map_err(|_error| Error::Configuration("origin"))?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::Configuration("origin"));
        }
        Ok(Self(url))
    }

    /// Returns the canonical origin used as the credential-store namespace.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub(crate) fn endpoint(&self, path: &str) -> Result<Url, Error> {
        self.0
            .join(path)
            .map_err(|_error| Error::Configuration("endpoint"))
    }
}

/// Required timeout and response allocation limits for one client.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub request_timeout: Duration,
    pub response_bytes: usize,
}

impl Limits {
    pub(crate) fn validate(&self) -> Result<(), Error> {
        if self.request_timeout.is_zero() || self.request_timeout > MAX_REQUEST_TIMEOUT {
            return Err(Error::Configuration("request_timeout"));
        }
        if self.response_bytes == 0 || self.response_bytes > MAX_RESPONSE_BYTES {
            return Err(Error::Configuration("response_bytes"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_cannot_carry_secrets_or_redirect_coordinates() {
        for value in [
            "http://relay.example",
            "https://user:secret@relay.example",
            "https://relay.example/path",
            "https://relay.example?token=secret",
            "https://relay.example/#fragment",
            "file:///tmp/socket",
        ] {
            Origin::parse(value).expect_err("unsafe origin");
        }
        assert_eq!(
            Origin::parse("https://RELAY.example:443")
                .expect("origin")
                .as_str(),
            "https://relay.example/"
        );
    }

    #[test]
    fn client_limits_fail_fast() {
        Limits {
            request_timeout: Duration::ZERO,
            response_bytes: 1024,
        }
        .validate()
        .expect_err("zero timeout");
        Limits {
            request_timeout: Duration::from_secs(1),
            response_bytes: MAX_RESPONSE_BYTES + 1,
        }
        .validate()
        .expect_err("large response limit");
    }
}
