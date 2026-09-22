//! Authenticates relay humans and credentials.
//!
//! The module keeps raw OAuth exchanges and relay bearer secrets out of
//! `PostgreSQL`. Durable rows contain keyed digests; pending device codes and
//! PKCE verifiers live only for the lifetime of this relay process.

// Rust guideline compliant 2026-09-08

mod oidc;
mod pending;
mod secret;
pub(crate) mod service;

#[cfg(all(test, feature = "postgres-tests"))]
mod parent_integrity_tests;

use secret::SecretValue;

#[doc(inline)]
pub use secret::DigestKey;

#[doc(inline)]
pub use oidc::{OidcClient, OidcClientConfig, OidcDeviceAuthorization, OidcIdentity};

#[doc(inline)]
pub use service::{
    link::{BrowserLinkStart, DeviceLinkStart, LinkedIdentity},
    AuthLimits, AuthService, AuthenticatedActor, BrowserCallbackResult, BrowserLoginStart,
    DeviceLoginStart, IssuedBrowserSession,
};

use std::fmt::{Debug, Formatter};

use thiserror::Error;

/// Reports a safe authentication failure.
#[derive(Debug, Error)]
pub enum AuthError {
    /// A supplied authentication value was malformed.
    #[error("authentication request is malformed")]
    Malformed,
    /// The configured OIDC issuer could not be contacted safely.
    #[error("configured OIDC issuer is unavailable")]
    IssuerUnavailable,
    /// An OIDC transaction was not current or did not validate.
    #[error("OIDC authentication transaction is invalid")]
    OidcInvalid,
    /// A credential or browser session is not current.
    #[error("authentication credential is not current")]
    CredentialInvalid,
    /// A browser request did not satisfy its CSRF binding.
    #[error("browser CSRF binding is invalid")]
    CsrfInvalid,
    /// A browser request did not have the configured origin.
    #[error("browser origin is not permitted")]
    OriginInvalid,
    /// Device authorization remains pending.
    #[error("device authorization remains pending")]
    DevicePending { retry_after_seconds: u32 },
    /// Device authorization remains pending and requires a longer interval.
    #[error("device authorization requires a slower polling interval")]
    DeviceSlowDown { retry_after_seconds: u32 },
    /// Device authorization was denied by the issuer.
    #[error("device authorization was denied")]
    DeviceDenied,
    /// Device authorization expired or was lost on process restart.
    #[error("device authorization expired")]
    DeviceExpired,
    /// A concurrent poll currently owns this device transaction.
    #[error("device authorization is already being polled")]
    DeviceBusy { retry_after_seconds: u32 },
    /// A bounded in-memory pending transaction store is full.
    #[error("authentication transaction capacity is exhausted")]
    Capacity,
    /// A durable operation failed without exposing its database detail.
    #[error("durable authentication state is unavailable")]
    Durable,
    /// A serializable identity transaction must be retried internally.
    #[error("durable authentication state is unavailable")]
    Retryable,
    /// A retry key was reused for a different credential mutation.
    #[error("credential mutation idempotency key conflicts with the original request")]
    IdempotencyConflict,
    /// A new credential rotation request has already passed its requested expiry.
    #[error("credential rotation request has expired")]
    RotationExpired,
    /// A credential rotation conflicts with the current configured policy.
    #[error("credential rotation request is rejected by policy")]
    RotationRejected,
    /// The caller has no stable OIDC identity that can prove an account link.
    #[error("account linking requires an authenticated human identity")]
    LinkUnsupportedActor,
    /// A browser transaction was presented to a bearer caller, or the reverse.
    #[error("account link transaction requires its original authentication channel")]
    LinkChannelMismatch,
    /// The account already has an open link transaction.
    #[error("account already has a pending link transaction")]
    LinkPending,
    /// No link transaction matches the requested coordinate.
    #[error("account link transaction was not found")]
    LinkNotFound,
    /// The link transaction passed its bounded expiry.
    #[error("account link transaction expired")]
    LinkExpired,
    /// The link transaction was cancelled before it could be proven.
    #[error("account link transaction was cancelled")]
    LinkCancelled,
    /// A link coordinate no longer matches current durable authority.
    #[error("account link transaction is no longer current")]
    LinkStale,
    /// A completion attempt did not come from the initiating principal.
    #[error("account link completion does not match its initiating account")]
    LinkCrossPrincipal,
    /// The transaction, callback, or identity proof was already consumed.
    #[error("account link transaction was already completed")]
    LinkReplayed,
    /// The proven identity is already linked to this account.
    #[error("account link target is already linked to this account")]
    LinkSelf,
    /// The proven identity is already linked to another account.
    #[error("account link target identity belongs to another account")]
    LinkCollision,
    /// Recovery quarantine forbids changing account authority.
    #[error("relay recovery quarantine forbids account link changes")]
    LinkQuarantined,
    /// Removing this identity would leave the account unable to authenticate.
    #[error("account must retain at least one linked identity")]
    UnlinkLastIdentity,
    /// No active linked identity matches the requested coordinate.
    #[error("linked identity was not found")]
    IdentityNotFound,
}

/// Identifies an opaque browser session cookie.
pub struct BrowserCookie(SecretValue);

impl BrowserCookie {
    /// Creates a browser cookie wrapper from a received cookie value.
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(SecretValue::new(value))
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.expose()
    }
}

impl Debug for BrowserCookie {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserCookie")
            .field("redacted", &true)
            .finish()
    }
}

/// Identifies a native or service bearer credential.
pub struct RelayBearerCredential(SecretValue);

impl RelayBearerCredential {
    /// Creates a bearer wrapper from an Authorization header value.
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(SecretValue::new(value))
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.expose()
    }

    pub(crate) fn parts(&self) -> Result<(&str, &str), AuthError> {
        let (public_id, secret) = self
            .0
            .expose()
            .split_once('.')
            .ok_or(AuthError::CredentialInvalid)?;
        if public_id.is_empty() || secret.is_empty() || secret.contains('.') {
            return Err(AuthError::CredentialInvalid);
        }
        relay_protocol::CredentialId::parse(public_id)
            .map_err(|_error| AuthError::CredentialInvalid)?;
        Ok((public_id, secret))
    }
}

impl Debug for RelayBearerCredential {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayBearerCredential")
            .field("redacted", &true)
            .finish()
    }
}

/// Identifies a device-polling possession secret.
pub struct DevicePollSecret(SecretValue);

impl DevicePollSecret {
    /// Creates a polling-secret wrapper from the dedicated request header.
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(SecretValue::new(value))
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.expose()
    }
}

/// Identifies the browser that initiated an unauthenticated login transaction.
pub struct LoginBindingCookie(SecretValue);

impl LoginBindingCookie {
    /// Creates a wrapper from the host-only login-binding cookie.
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(SecretValue::new(value))
    }
    pub(crate) fn expose(&self) -> &str {
        self.0.expose()
    }
}

impl Debug for LoginBindingCookie {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginBindingCookie")
            .field("redacted", &true)
            .finish()
    }
}

/// Holds the one-use authorization code received from the provider callback.
pub struct OidcCallbackCode(SecretValue);

impl OidcCallbackCode {
    /// Wraps a callback code before it enters the relay authentication boundary.
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(SecretValue::new(value))
    }
    pub(crate) fn expose(&self) -> &str {
        self.0.expose()
    }
}

impl Debug for OidcCallbackCode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcCallbackCode")
            .field("redacted", &true)
            .finish()
    }
}

/// Provides the provider result without retaining callback diagnostics.
pub enum BrowserCallbackOutcome {
    /// A one-use provider code is available for the PKCE exchange.
    Code(OidcCallbackCode),
    /// The provider denied a bound browser login.
    ProviderDenied,
    /// The callback carried an invalid combination of provider coordinates.
    Invalid,
}

impl Debug for BrowserCallbackOutcome {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserCallbackOutcome")
            .field("redacted", &true)
            .finish()
    }
}

/// Callback coordinates that must be checked after safely binding its state.
pub struct BrowserCallback {
    state: String,
    issuer: Option<String>,
    session_state: Option<String>,
    outcome: BrowserCallbackOutcome,
}

impl BrowserCallback {
    /// Creates a callback value from bounded HTTP coordinates.
    #[must_use]
    pub fn new(
        state: String,
        issuer: Option<String>,
        session_state: Option<String>,
        outcome: BrowserCallbackOutcome,
    ) -> Self {
        Self {
            state,
            issuer,
            session_state,
            outcome,
        }
    }
}

impl Debug for BrowserCallback {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserCallback")
            .field("redacted", &true)
            .finish()
    }
}

impl Debug for DevicePollSecret {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevicePollSecret")
            .field("redacted", &true)
            .finish()
    }
}

/// Provides a one-time relay credential without diagnostic exposure.
pub struct IssuedCredential {
    /// Safe public identifier used to select the stored credential row.
    pub public_id: String,
    secret: SecretValue,
    expires_at: time::OffsetDateTime,
}

impl IssuedCredential {
    /// Creates one protected credential delivery value.
    #[must_use]
    pub fn new(public_id: String, secret: String, expires_at: time::OffsetDateTime) -> Self {
        Self {
            public_id,
            secret: SecretValue::new(secret),
            expires_at,
        }
    }

    /// Borrows the one-time secret for the protected native delivery channel.
    #[must_use]
    pub fn secret(&self) -> &str {
        self.secret.expose()
    }

    /// Returns the durable credential expiry selected by the issuing transaction.
    #[must_use]
    pub const fn expires_at(&self) -> time::OffsetDateTime {
        self.expires_at
    }
}

impl Debug for IssuedCredential {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedCredential")
            .field("public_id", &self.public_id)
            .field("secret", &"<redacted>")
            .finish_non_exhaustive()
    }
}
