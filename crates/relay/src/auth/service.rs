//! Resolves current relay authentication from durable digest rows.

// Rust guideline compliant 2026-09-08

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use relay_protocol::{
    AccountRecord, CreateServiceAccountRequest, CredentialId, CredentialKind, CredentialMutation,
    CredentialPage, CredentialRecord, DeviceCredential, IdentityRecord, PageRequest, PrincipalId,
    PrincipalKind, PrincipalState, RotateCredentialRequest, Secret, ServiceAccountCreated,
    ServiceAccountPage, ServiceAccountRecord, TeamId,
};
use sqlx::{Postgres, Row, Transaction};
use tokio::sync::Mutex;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    admission::{AuthMutationTarget, Authority, AuthorityError},
    config::LoginPolicy,
    store::{ActorKind, AuthenticationBinding, Store, StoreError},
};

use super::{
    oidc::{OidcClient, OidcDeviceAuthorization},
    pending::{PendingDeviceCode, PendingDeviceCodes},
    AuthError, BrowserCallback, BrowserCallbackOutcome, BrowserCookie, DevicePollSecret, DigestKey,
    LoginBindingCookie, RelayBearerCredential,
};

const RANDOM_SECRET_BYTES: usize = 32;
const IDENTITY_ADVISORY_LOCK_SEED: i64 = 0;
/// Retries absorb normal PostgreSQL serializable conflicts without unbounded ingress work.
const IDENTITY_ISSUE_MAX_ATTEMPTS: usize = 3;
/// Provider session coordinates are opaque and bounded before durable processing.
const MAX_CALLBACK_SESSION_STATE_BYTES: usize = 4 * 1024;

/// Validated lifetimes and polling bounds for relay authentication state.
///
/// The server derives these values from required relay configuration. Keeping
/// them together prevents an authentication path from silently choosing an
/// independent credential lifetime.
#[derive(Debug, Clone)]
pub struct AuthLimits {
    login_lifetime: Duration,
    browser_session_lifetime: Duration,
    browser_session_idle: Duration,
    human_credential_lifetime: Duration,
    service_credential_lifetime: Duration,
    max_rotation_overlap: Duration,
    credentials_per_principal: usize,
    service_accounts_per_team: usize,
    device_poll_lease: Duration,
    device_slow_down_increment: Duration,
    max_device_poll_interval: Duration,
}

impl AuthLimits {
    /// Validates the explicitly configured authentication limits.
    ///
    /// # Errors
    /// Returns [`AuthError::Malformed`] when a lifetime is zero, an idle
    /// lifetime can outlive its session, or a poll bound is incoherent.
    pub fn new(
        login_lifetime: Duration,
        browser_session_lifetime: Duration,
        browser_session_idle: Duration,
        human_credential_lifetime: Duration,
        service_credential_lifetime: Duration,
        max_rotation_overlap: Duration,
        credentials_per_principal: usize,
        service_accounts_per_team: usize,
        device_poll_lease: Duration,
        device_slow_down_increment: Duration,
        max_device_poll_interval: Duration,
    ) -> Result<Self, AuthError> {
        if login_lifetime.is_zero()
            || browser_session_lifetime.is_zero()
            || browser_session_idle.is_zero()
            || human_credential_lifetime.is_zero()
            || service_credential_lifetime.is_zero()
            || max_rotation_overlap.is_zero()
            || credentials_per_principal == 0
            || service_accounts_per_team == 0
            || device_poll_lease.is_zero()
            || device_slow_down_increment.is_zero()
            || max_device_poll_interval.is_zero()
            || browser_session_idle > browser_session_lifetime
            || device_slow_down_increment > max_device_poll_interval
            || max_rotation_overlap > human_credential_lifetime
            || max_rotation_overlap > service_credential_lifetime
        {
            return Err(AuthError::Malformed);
        }
        Ok(Self {
            login_lifetime,
            browser_session_lifetime,
            browser_session_idle,
            human_credential_lifetime,
            service_credential_lifetime,
            max_rotation_overlap,
            credentials_per_principal,
            service_accounts_per_team,
            device_poll_lease,
            device_slow_down_increment,
            max_device_poll_interval,
        })
    }

    /// Returns the configured native-human credential lifetime.
    #[must_use]
    pub const fn human_credential_lifetime(&self) -> Duration {
        self.human_credential_lifetime
    }

    /// Returns the configured service credential lifetime.
    #[must_use]
    pub const fn service_credential_lifetime(&self) -> Duration {
        self.service_credential_lifetime
    }

    /// Returns the maximum overlap permitted for a replacement credential.
    #[must_use]
    pub const fn max_rotation_overlap(&self) -> Duration {
        self.max_rotation_overlap
    }

    /// Returns the per-principal active credential limit.
    #[must_use]
    pub const fn credentials_per_principal(&self) -> usize {
        self.credentials_per_principal
    }

    /// Returns the per-team service account limit.
    #[must_use]
    pub const fn service_accounts_per_team(&self) -> usize {
        self.service_accounts_per_team
    }
}

struct PendingBrowserLogin {
    verifier: Zeroizing<String>,
    nonce: Zeroizing<String>,
    expires_at: Instant,
}

struct DeviceIssueBinding {
    issuer: String,
    client_id: String,
    login_id: Uuid,
    recovery_generation: i64,
    poll_interval_seconds: u32,
    poll_lease_token: [u8; RANDOM_SECRET_BYTES],
}

#[derive(Clone, Copy)]
struct HumanIdentity {
    principal_id: Uuid,
    identity_id: Uuid,
}

impl std::fmt::Debug for PendingBrowserLogin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingBrowserLogin")
            .field("redacted", &true)
            .finish()
    }
}

/// Redirect target and opaque correlation coordinate for a browser login.
#[derive(Clone)]
pub struct BrowserLoginStart {
    pub login_id: Uuid,
    pub authorization_url: url::Url,
}

impl std::fmt::Debug for BrowserLoginStart {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrowserLoginStart")
            .field("login_id", &self.login_id)
            .field("redacted", &true)
            .finish()
    }
}

/// RFC 8628 values sent once to a native client, including its poll possession secret.
pub struct DeviceLoginStart {
    pub login_id: Uuid,
    pub authorization: OidcDeviceAuthorization,
    poll_secret: super::SecretValue,
}
impl DeviceLoginStart {
    #[must_use]
    pub fn poll_secret(&self) -> &str {
        self.poll_secret.expose()
    }
}
impl std::fmt::Debug for DeviceLoginStart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceLoginStart")
            .field("login_id", &self.login_id)
            .field("redacted", &true)
            .finish()
    }
}

/// Opaque browser session values delivered only through protected response cookies.
pub struct IssuedBrowserSession {
    pub session_id: Uuid,
    cookie: super::SecretValue,
    csrf: super::SecretValue,
    expires_at: time::OffsetDateTime,
}

impl IssuedBrowserSession {
    #[must_use]
    pub fn cookie(&self) -> &str {
        self.cookie.expose()
    }
    #[must_use]
    pub fn csrf(&self) -> &str {
        self.csrf.expose()
    }
    /// Returns the durable absolute session expiry.
    #[must_use]
    pub const fn expires_at(&self) -> time::OffsetDateTime {
        self.expires_at
    }
}
impl std::fmt::Debug for IssuedBrowserSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IssuedBrowserSession")
            .field("session_id", &self.session_id)
            .field("redacted", &true)
            .finish()
    }
}

/// Carries an actor proven by one current relay credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthenticatedActor {
    actor: crate::store::ActorContext,
    identity_id: Option<Uuid>,
}

impl AuthenticatedActor {
    /// Returns the non-forgeable store context for an authenticated caller.
    #[must_use]
    pub const fn actor(self) -> crate::store::ActorContext {
        self.actor
    }

    /// Returns the durable OIDC identity that proved this human actor.
    #[must_use]
    pub const fn identity_id(self) -> Option<Uuid> {
        self.identity_id
    }
}

/// Resolves opaque browser and bearer authentication using PostgreSQL state.
#[derive(Debug, Clone)]
pub struct AuthService {
    store: Store,
    digest_key: DigestKey,
    pending_device_codes: PendingDeviceCodes,
    pending_browser_logins: Arc<Mutex<HashMap<Uuid, PendingBrowserLogin>>>,
    pending_capacity: usize,
    limits: AuthLimits,
    login_policy: LoginPolicy,
    authority: Arc<Authority>,
    #[cfg(test)]
    rotation_hook: Arc<Mutex<Option<RotationHook>>>,
    #[cfg(test)]
    rotation_retry_count: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    revoke_hook: Arc<Mutex<Option<RotationHook>>>,
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct RotationHook {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl AuthService {
    /// Creates the relay authentication service with bounded transient device state.
    #[must_use]
    pub fn new(
        store: Store,
        digest_key: DigestKey,
        pending_device_capacity: usize,
        limits: AuthLimits,
        login_policy: LoginPolicy,
        authority: Arc<Authority>,
    ) -> Self {
        Self {
            store,
            digest_key,
            pending_device_codes: PendingDeviceCodes::new(pending_device_capacity),
            pending_browser_logins: Arc::new(Mutex::new(HashMap::new())),
            pending_capacity: pending_device_capacity,
            limits,
            login_policy,
            authority,
            #[cfg(test)]
            rotation_hook: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            rotation_retry_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            revoke_hook: Arc::new(Mutex::new(None)),
        }
    }

    /// Creates and durably records a one-use, browser-bound PKCE login.
    pub async fn begin_browser_login(
        &self,
        oidc: &OidcClient,
        binding: LoginBindingCookie,
    ) -> Result<BrowserLoginStart, AuthError> {
        let authorization = oidc.begin_browser();
        let login_id = Uuid::now_v7();
        self.prune_expired_pending().await?;
        let mut pending = self.pending_browser_logins.lock().await;
        pending.retain(|_, login| login.expires_at > Instant::now());
        if pending.len() >= self.pending_capacity {
            return Err(AuthError::Capacity);
        }
        let redirect_uri = oidc.redirect_uri().ok_or(AuthError::OidcInvalid)?;
        let mut transaction = self
            .store
            .begin_serializable()
            .await
            .map_err(|_| AuthError::Durable)?;
        self.authority
            .verify_fence_in_transaction(&mut transaction)
            .await
            .map_err(|_| AuthError::Durable)?;
        let inserted = sqlx::query(
            "INSERT INTO browser_logins (login_id, state_digest, nonce_digest, pkce_verifier_digest, login_binding_digest, issuer, client_id, audience, redirect_uri, action, account_link_generation, recovery_generation, expires_at) \
             SELECT $1, $2, $3, $4, $5, $6, $7, $7, $8, 'login', 1, r.recovery_generation, clock_timestamp() + $9::interval \
             FROM relay_identity r WHERE r.state = 'normal'",
        )
        .bind(login_id).bind(self.digest_key.digest(&authorization.state).as_slice())
        .bind(self.digest_key.digest(&authorization.nonce).as_slice())
        .bind(self.digest_key.digest(&authorization.verifier).as_slice())
        .bind(self.digest_key.digest(binding.expose()).as_slice())
        .bind(oidc.issuer()).bind(oidc.client_id()).bind(redirect_uri)
        .bind(interval(self.limits.login_lifetime))
        .execute(&mut *transaction).await.map_err(database_error)?;
        if inserted.rows_affected() != 1 {
            return Err(AuthError::Durable);
        }
        audit_auth(
            &mut transaction,
            "auth.browser.begin",
            "changed",
            "browser_login",
            "committed",
            None,
        )
        .await?;
        self.commit_current(transaction).await?;
        pending.insert(
            login_id,
            PendingBrowserLogin {
                verifier: authorization.verifier,
                nonce: authorization.nonce,
                expires_at: Instant::now() + self.limits.login_lifetime,
            },
        );
        Ok(BrowserLoginStart {
            login_id,
            authorization_url: authorization.url,
        })
    }

    /// Begins the mandatory device grant and persists only its keyed boundaries.
    pub async fn begin_device_login(
        &self,
        oidc: &OidcClient,
    ) -> Result<DeviceLoginStart, AuthError> {
        let reservation = self.pending_device_codes.reserve()?;
        let authorization = oidc.begin_device().await?;
        let login_id = Uuid::now_v7();
        let poll_secret = random_secret()?;
        self.prune_expired_pending().await?;
        let mut transaction = self
            .store
            .begin_serializable()
            .await
            .map_err(|_| AuthError::Durable)?;
        self.authority
            .verify_fence_in_transaction(&mut transaction)
            .await
            .map_err(|_| AuthError::Durable)?;
        let inserted = sqlx::query(
            "INSERT INTO device_logins (login_id, device_code_digest, poll_secret_digest, issuer, client_id, audience, action, account_link_generation, recovery_generation, expires_at, poll_interval_seconds, next_poll_at) \
             SELECT $1, $2, $3, $4, $5, $5, 'login', 1, r.recovery_generation, clock_timestamp() + $6::interval, $7, clock_timestamp() \
             FROM relay_identity r WHERE r.state = 'normal'",
        ).bind(login_id).bind(self.digest_key.digest(authorization.device_code()).as_slice()).bind(self.digest_key.digest(&poll_secret).as_slice())
        .bind(oidc.issuer()).bind(oidc.client_id()).bind(interval(authorization.expires_in))
        .bind(i32::try_from(authorization.interval.as_secs()).map_err(|_| AuthError::Malformed)?)
        .execute(&mut *transaction).await.map_err(database_error)?;
        if inserted.rows_affected() != 1 {
            return Err(AuthError::Durable);
        }
        audit_auth(
            &mut transaction,
            "auth.device.begin",
            "changed",
            "device_login",
            "committed",
            None,
        )
        .await?;
        self.commit_current(transaction).await?;
        if let Err(error) =
            reservation.commit(login_id, PendingDeviceCode::new(authorization.clone()))
        {
            self.reject_device_login(login_id, "cancelled", None)
                .await?;
            return Err(error);
        }
        Ok(DeviceLoginStart {
            login_id,
            authorization,
            poll_secret: super::SecretValue::new(poll_secret),
        })
    }

    /// Consumes a callback exactly once before exchanging its authorization code.
    pub async fn complete_browser_login(
        &self,
        oidc: &OidcClient,
        callback: BrowserCallback,
        binding: LoginBindingCookie,
    ) -> Result<IssuedBrowserSession, AuthError> {
        let mut transaction = self
            .store
            .begin_serializable()
            .await
            .map_err(|_| AuthError::Durable)?;
        self.authority
            .verify_fence_in_transaction(&mut transaction)
            .await
            .map_err(|_| AuthError::Durable)?;
        let row = sqlx::query(
            "UPDATE browser_logins b SET consumed_at = clock_timestamp(), outcome = 'failed' \
             FROM relay_identity r WHERE b.state_digest = $1 AND b.login_binding_digest = $2 \
             AND b.issuer = $3 AND b.client_id = $4 AND b.audience = $4 AND b.redirect_uri = $5 AND b.action = 'login' \
             AND b.link_id IS NULL AND b.account_link_generation = 1 \
             AND b.consumed_at IS NULL AND b.expires_at > clock_timestamp() AND b.recovery_generation = r.recovery_generation AND r.state = 'normal' \
             RETURNING b.login_id, b.nonce_digest, b.pkce_verifier_digest, b.issuer, b.client_id, b.audience, b.redirect_uri, b.action, b.link_id, b.account_link_generation, b.recovery_generation",
        ).bind(self.digest_key.digest(&callback.state).as_slice()).bind(self.digest_key.digest(binding.expose()).as_slice())
        .bind(oidc.issuer()).bind(oidc.client_id()).bind(oidc.redirect_uri().ok_or(AuthError::OidcInvalid)?)
        .fetch_optional(&mut *transaction).await.map_err(database_error)?;
        let Some(row) = row else {
            audit_auth(
                &mut transaction,
                "auth.browser.callback",
                "deny",
                "browser_login",
                "rejected",
                None,
            )
            .await?;
            self.commit_current(transaction).await?;
            return Err(AuthError::OidcInvalid);
        };
        let login_id: Uuid = row.get("login_id");
        let pending = self.remove_pending_browser_login(login_id).await;
        if row.get::<String, _>("issuer") != oidc.issuer()
            || row.get::<String, _>("client_id") != oidc.client_id()
            || row.get::<String, _>("audience") != oidc.client_id()
            || row.get::<String, _>("redirect_uri")
                != oidc.redirect_uri().ok_or(AuthError::OidcInvalid)?
            || row.get::<String, _>("action") != "login"
            || row.get::<Option<Uuid>, _>("link_id").is_some()
            || row.get::<i64, _>("account_link_generation") != 1
            || callback
                .issuer
                .as_deref()
                .is_some_and(|issuer| issuer != oidc.issuer())
            || callback.session_state.as_ref().is_some_and(|value| {
                value.is_empty() || value.len() > MAX_CALLBACK_SESSION_STATE_BYTES
            })
            || !matches!(callback.outcome, BrowserCallbackOutcome::Code(_))
        {
            audit_auth(
                &mut transaction,
                "auth.browser.callback",
                "deny",
                "browser_login",
                "callback_invalid",
                Some(row.get("recovery_generation")),
            )
            .await?;
            self.commit_current(transaction).await?;
            return Err(AuthError::OidcInvalid);
        }
        let Some(pending) = pending else {
            audit_auth(
                &mut transaction,
                "auth.browser.callback",
                "deny",
                "browser_login",
                "verifier_missing",
                Some(row.get("recovery_generation")),
            )
            .await?;
            self.commit_current(transaction).await?;
            return Err(AuthError::OidcInvalid);
        };
        if !self.digest_key.matches(
            &pending.nonce,
            row.get::<Vec<u8>, _>("nonce_digest").as_slice(),
        ) || !self.digest_key.matches(
            &pending.verifier,
            row.get::<Vec<u8>, _>("pkce_verifier_digest").as_slice(),
        ) {
            audit_auth(
                &mut transaction,
                "auth.browser.callback",
                "deny",
                "browser_login",
                "binding_invalid",
                Some(row.get("recovery_generation")),
            )
            .await?;
            self.commit_current(transaction).await?;
            return Err(AuthError::OidcInvalid);
        }
        audit_auth(
            &mut transaction,
            "auth.browser.callback",
            "changed",
            "browser_login",
            "consumed",
            Some(row.get("recovery_generation")),
        )
        .await?;
        self.commit_current(transaction).await?;
        let BrowserCallbackOutcome::Code(code) = callback.outcome else {
            return Err(AuthError::OidcInvalid);
        };
        let identity = match oidc
            .exchange_browser_code(code.expose().to_owned(), pending.verifier, pending.nonce)
            .await
        {
            Ok(identity) => identity,
            Err(_) => {
                self.audit_rejection("auth.browser.callback", "browser_login", "provider_invalid")
                    .await?;
                return Err(AuthError::OidcInvalid);
            }
        };
        if identity.issuer != oidc.issuer() {
            self.audit_rejection("auth.browser.callback", "browser_login", "issuer_invalid")
                .await?;
            return Err(AuthError::OidcInvalid);
        }
        let recovery_generation: i64 = row.get("recovery_generation");
        self.issue_browser_session(
            identity.issuer,
            identity.subject,
            oidc.client_id().to_owned(),
            oidc.redirect_uri()
                .ok_or(AuthError::OidcInvalid)?
                .to_owned(),
            login_id,
            recovery_generation,
        )
        .await
    }

    /// Owns one due device poll, exchanges it once, and delivers a relay credential once.
    pub async fn poll_device_login(
        &self,
        oidc: &OidcClient,
        login_id: Uuid,
        secret: DevicePollSecret,
    ) -> Result<super::IssuedCredential, AuthError> {
        let row = sqlx::query("SELECT poll_secret_digest, issuer, client_id, audience, action, link_id, account_link_generation, recovery_generation, poll_interval_seconds FROM device_logins WHERE login_id = $1 AND consumed_at IS NULL AND expires_at > clock_timestamp()")
            .bind(login_id).fetch_optional(self.store.pool()).await.map_err(database_error)?;
        let Some(row) = row else {
            self.audit_rejection("auth.device.poll", "device_login", "expired")
                .await?;
            return Err(AuthError::DeviceExpired);
        };
        let poll_interval_seconds = interval_seconds(row.get("poll_interval_seconds"))?;
        if !self.digest_key.matches(
            secret.expose(),
            row.get::<Vec<u8>, _>("poll_secret_digest").as_slice(),
        ) {
            self.audit_rejection("auth.device.poll", "device_login", "possession_invalid")
                .await?;
            return Err(AuthError::CredentialInvalid);
        }
        if row.get::<String, _>("issuer") != oidc.issuer()
            || row.get::<String, _>("client_id") != oidc.client_id()
            || row.get::<String, _>("audience") != oidc.client_id()
            || row.get::<String, _>("action") != "login"
            || row.get::<Option<Uuid>, _>("link_id").is_some()
            || row.get::<i64, _>("account_link_generation") != 1
        {
            self.reject_device_login(login_id, "failed", None).await?;
            return Err(AuthError::OidcInvalid);
        }
        let poll_lease_token = random_token()?;
        let mut claim_transaction = self
            .store
            .begin_serializable()
            .await
            .map_err(|_| AuthError::Durable)?;
        self.authority
            .verify_fence_in_transaction(&mut claim_transaction)
            .await
            .map_err(|_| AuthError::Durable)?;
        let claimed = sqlx::query("UPDATE device_logins d SET poll_lease_until = clock_timestamp() + $2::interval, poll_lease_token = $3 FROM relay_identity r WHERE d.login_id = $1 AND d.issuer = $4 AND d.client_id = $5 AND d.audience = $5 AND d.action = 'login' AND d.link_id IS NULL AND d.account_link_generation = 1 AND d.consumed_at IS NULL AND d.expires_at > clock_timestamp() AND d.next_poll_at <= clock_timestamp() AND (d.poll_lease_until IS NULL OR d.poll_lease_until < clock_timestamp()) AND d.recovery_generation = r.recovery_generation AND r.state = 'normal' RETURNING d.recovery_generation")
            .bind(login_id).bind(interval(self.limits.device_poll_lease)).bind(poll_lease_token.as_slice()).bind(oidc.issuer()).bind(oidc.client_id()).fetch_optional(&mut *claim_transaction).await.map_err(database_error)?.ok_or(AuthError::DeviceBusy { retry_after_seconds: poll_interval_seconds })?;
        self.commit_current(claim_transaction).await?;
        let recovery_generation: i64 = claimed.get("recovery_generation");
        let pending = self.pending_device_codes.take(login_id).await;
        let Some(pending) = pending else {
            self.reject_device_login(login_id, "expired", Some(&poll_lease_token))
                .await?;
            return Err(AuthError::DeviceExpired);
        };
        let identity = match oidc.poll_device(pending.authorization()).await {
            Ok(identity) => identity,
            Err(AuthError::DevicePending { .. }) => {
                self.pending_device_codes.restore(login_id, pending).await;
                let mut transaction = self
                    .store
                    .begin_serializable()
                    .await
                    .map_err(|_| AuthError::Durable)?;
                self.authority
                    .verify_fence_in_transaction(&mut transaction)
                    .await
                    .map_err(|_| AuthError::Durable)?;
                let updated = sqlx::query("UPDATE device_logins SET poll_lease_until = NULL, poll_lease_token = NULL, next_poll_at = clock_timestamp() + make_interval(secs => poll_interval_seconds) WHERE login_id = $1 AND poll_lease_token = $2 AND consumed_at IS NULL AND expires_at > clock_timestamp() RETURNING poll_interval_seconds")
                    .bind(login_id).bind(poll_lease_token.as_slice()).fetch_optional(&mut *transaction).await.map_err(database_error)?;
                self.commit_current(transaction).await?;
                let Some(updated) = updated else {
                    return Err(AuthError::DeviceBusy {
                        retry_after_seconds: poll_interval_seconds,
                    });
                };
                return Err(AuthError::DevicePending {
                    retry_after_seconds: interval_seconds(updated.get("poll_interval_seconds"))?,
                });
            }
            Err(AuthError::DeviceSlowDown { .. }) => {
                self.pending_device_codes.restore(login_id, pending).await;
                let mut transaction = self
                    .store
                    .begin_serializable()
                    .await
                    .map_err(|_| AuthError::Durable)?;
                self.authority
                    .verify_fence_in_transaction(&mut transaction)
                    .await
                    .map_err(|_| AuthError::Durable)?;
                let updated = sqlx::query(
                    "UPDATE device_logins SET poll_lease_until = NULL, poll_lease_token = NULL, \
                     poll_interval_seconds = LEAST(poll_interval_seconds + $2, $3), \
                     next_poll_at = clock_timestamp() + make_interval(secs => LEAST(poll_interval_seconds + $2, $3)) \
                     WHERE login_id = $1 AND poll_lease_token = $4 AND consumed_at IS NULL AND expires_at > clock_timestamp() \
                     RETURNING poll_interval_seconds",
                )
                .bind(login_id)
                .bind(seconds_i32(self.limits.device_slow_down_increment)?)
                .bind(seconds_i32(self.limits.max_device_poll_interval)?)
                .bind(poll_lease_token.as_slice())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(database_error)?;
                self.commit_current(transaction).await?;
                let Some(updated) = updated else {
                    return Err(AuthError::DeviceBusy {
                        retry_after_seconds: poll_interval_seconds,
                    });
                };
                return Err(AuthError::DeviceSlowDown {
                    retry_after_seconds: interval_seconds(updated.get("poll_interval_seconds"))?,
                });
            }
            Err(error) => {
                let outcome = match error {
                    AuthError::DeviceDenied => "denied",
                    AuthError::DeviceExpired => "expired",
                    _ => "failed",
                };
                self.reject_device_login(login_id, outcome, Some(&poll_lease_token))
                    .await?;
                return Err(error);
            }
        };
        if identity.issuer != oidc.issuer() {
            self.reject_device_login(login_id, "failed", Some(&poll_lease_token))
                .await?;
            return Err(AuthError::OidcInvalid);
        }
        self.issue_human_credential(
            identity.subject,
            DeviceIssueBinding {
                issuer: identity.issuer,
                client_id: oidc.client_id().to_owned(),
                login_id,
                recovery_generation,
                poll_interval_seconds,
                poll_lease_token,
            },
        )
        .await
    }

    /// Authenticates one opaque browser session cookie.
    ///
    /// # Errors
    /// Returns [`AuthError::CredentialInvalid`] when the session, principal, or
    /// recovery generation is no longer current.
    pub async fn authenticate_browser(
        &self,
        cookie: BrowserCookie,
    ) -> Result<AuthenticatedActor, AuthError> {
        let digest = self.digest_key.digest(cookie.expose());
        let row = sqlx::query(
            "SELECT s.principal_id, s.identity_id, s.session_id, s.session_generation, s.recovery_generation, p.kind AS principal_kind \
             FROM browser_sessions s JOIN principals p ON p.id = s.principal_id \
             JOIN relay_identity r ON r.state = 'normal' \
             WHERE s.cookie_digest = $1 AND s.digest_key_id = $2 AND s.revoked_at IS NULL AND s.expires_at > clock_timestamp() \
             AND s.idle_deadline > clock_timestamp() AND p.state = 'active' \
             AND s.recovery_generation = r.recovery_generation",
        )
        .bind(digest.as_slice())
        .bind(self.digest_key.key_id())
        .fetch_optional(self.store.pool())
        .await
        .map_err(database_error)?
        .ok_or(AuthError::CredentialInvalid)?;
        let principal_id: Uuid = row.get("principal_id");
        let session_id: Uuid = row.get("session_id");
        let generation: i64 = row.get("session_generation");
        let recovery_generation: i64 = row.get("recovery_generation");
        let identity_id: Option<Uuid> = row.get("identity_id");
        let actor_kind = browser_actor_kind(row.get("principal_kind"))?;
        self.touch_browser_session(session_id).await?;
        Ok(AuthenticatedActor {
            actor: Store::authenticated_actor(
                principal_id,
                session_id,
                generation,
                recovery_generation,
                actor_kind,
                AuthenticationBinding::BrowserSession,
            ),
            identity_id,
        })
    }

    /// Verifies a supplied CSRF value against the durable session digest.
    pub async fn verify_browser_csrf(&self, cookie: &str, csrf: &str) -> Result<(), AuthError> {
        let cookie_digest = self.digest_key.digest(cookie);
        let csrf_digest = self.digest_key.digest(csrf);
        let present = sqlx::query_scalar::<_, Uuid>(
            "SELECT s.session_id FROM browser_sessions s JOIN relay_identity r ON r.state = 'normal' \
             WHERE s.cookie_digest = $1 AND s.csrf_digest = $2 AND s.digest_key_id = $3 \
             AND s.revoked_at IS NULL AND s.expires_at > clock_timestamp() AND s.idle_deadline > clock_timestamp() \
             AND s.recovery_generation = r.recovery_generation FOR SHARE",
        )
        .bind(cookie_digest.as_slice())
        .bind(csrf_digest.as_slice())
        .bind(self.digest_key.key_id())
        .fetch_optional(self.store.pool())
        .await
        .map_err(database_error)?;
        if present.is_some() {
            Ok(())
        } else {
            Err(AuthError::CsrfInvalid)
        }
    }

    /// Authenticates one native-human or service bearer credential.
    ///
    /// # Errors
    /// Returns [`AuthError::CredentialInvalid`] when the credential is expired,
    /// revoked, or no longer belongs to an active principal.
    pub async fn authenticate_bearer(
        &self,
        credential: RelayBearerCredential,
    ) -> Result<AuthenticatedActor, AuthError> {
        let (public_id, secret) = credential.parts()?;
        let digest = self.digest_key.digest(secret);
        let row = sqlx::query(
            "SELECT c.principal_id, c.identity_id, c.credential_id, c.credential_generation, c.recovery_generation, c.credential_kind, p.kind AS principal_kind \
             FROM relay_credentials c JOIN principals p ON p.id = c.principal_id \
             JOIN relay_identity r ON r.state = 'normal' \
             WHERE c.public_id = $1 AND c.secret_digest = $2 AND c.digest_key_id = $3 \
             AND c.revoked_at IS NULL AND c.expires_at > clock_timestamp() \
             AND (c.rotation_overlap_ends_at IS NULL OR c.rotation_overlap_ends_at > clock_timestamp()) \
             AND p.state = 'active' AND c.recovery_generation = r.recovery_generation \
             AND (c.credential_kind <> 'service' OR EXISTS (SELECT 1 FROM service_accounts s WHERE s.principal_id = c.principal_id AND s.deprovisioned_at IS NULL))",
        )
        .bind(public_id)
        .bind(digest.as_slice())
        .bind(self.digest_key.key_id())
        .fetch_optional(self.store.pool())
        .await
        .map_err(database_error)?
        .ok_or(AuthError::CredentialInvalid)?;
        let actor_kind =
            credential_actor_kind(row.get("principal_kind"), row.get("credential_kind"))?;
        let principal_id: Uuid = row.get("principal_id");
        let credential_id: Uuid = row.get("credential_id");
        let generation: i64 = row.get("credential_generation");
        let recovery_generation: i64 = row.get("recovery_generation");
        let identity_id: Option<Uuid> = row.get("identity_id");
        self.touch_credential(credential_id).await?;
        Ok(AuthenticatedActor {
            actor: Store::authenticated_actor(
                principal_id,
                credential_id,
                generation,
                recovery_generation,
                actor_kind,
                AuthenticationBinding::Credential,
            ),
            identity_id,
        })
    }

    /// Reads the account represented by a current, non-forgeable actor proof.
    pub async fn account(&self, actor: AuthenticatedActor) -> Result<AccountRecord, AuthError> {
        let context = actor.actor();
        let mut transaction = self
            .store
            .begin_serializable()
            .await
            .map_err(|_| AuthError::Durable)?;
        self.authority
            .verify_fence_in_transaction(&mut transaction)
            .await
            .map_err(|_| AuthError::Durable)?;
        crate::authorization::verify_current_actor(&mut transaction, context)
            .await
            .map_err(|_| AuthError::CredentialInvalid)?;
        let row = sqlx::query(
            "SELECT p.kind, p.state FROM principals p JOIN relay_identity r ON r.state = 'normal' \
             WHERE p.id = $1 AND p.state = 'active' AND r.recovery_generation = $2",
        )
        .bind(context.principal_id())
        .bind(context.recovery_generation())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?
        .ok_or(AuthError::CredentialInvalid)?;
        let identities = sqlx::query(
            "SELECT identity_id, issuer, subject FROM oidc_identities \
             WHERE principal_id = $1 AND removed_at IS NULL ORDER BY identity_id ASC LIMIT 128",
        )
        .bind(context.principal_id())
        .fetch_all(&mut *transaction)
        .await
        .map_err(database_error)?
        .into_iter()
        .map(|identity| IdentityRecord {
            identity_id: identity.get("identity_id"),
            issuer: identity.get("issuer"),
            subject: identity.get("subject"),
        })
        .collect();
        let result = AccountRecord {
            principal_id: PrincipalId::from_uuid(context.principal_id()),
            kind: principal_kind(row.get("kind"))?,
            state: principal_state(row.get("state"))?,
            identities,
        };
        self.commit_current(transaction).await?;
        Ok(result)
    }

    /// Lists bounded non-secret credential metadata owned by the authenticated account.
    pub async fn list_credentials(
        &self,
        actor: AuthenticatedActor,
        page: PageRequest,
    ) -> Result<CredentialPage, AuthError> {
        let limit = page_limit(page.limit)?;
        let context = actor.actor();
        let mut transaction = self
            .store
            .begin_serializable()
            .await
            .map_err(|_| AuthError::Durable)?;
        self.authority
            .verify_fence_in_transaction(&mut transaction)
            .await
            .map_err(|_| AuthError::Durable)?;
        crate::authorization::verify_current_actor(&mut transaction, context)
            .await
            .map_err(|_| AuthError::CredentialInvalid)?;
        let mut records = sqlx::query(
            "SELECT c.credential_id, c.principal_id, c.credential_kind, c.issued_at, c.expires_at, \
             c.rotation_overlap_ends_at, c.last_used_at, c.revoked_at \
             FROM relay_credentials c JOIN relay_identity r ON r.state = 'normal' \
             WHERE c.principal_id = $1 AND c.recovery_generation = r.recovery_generation \
             AND ($2::uuid IS NULL OR c.credential_id > $2) ORDER BY c.credential_id ASC LIMIT $3",
        )
        .bind(context.principal_id())
        .bind(page.after)
        .bind(limit + 1)
        .fetch_all(&mut *transaction)
        .await
        .map_err(database_error)?
        .into_iter()
        .map(credential_record)
        .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = page_cursor(&mut records, limit);
        self.commit_current(transaction).await?;
        Ok(CredentialPage {
            records,
            next_cursor,
        })
    }

    /// Revokes one credential owned by the authenticated account.
    pub async fn revoke_credential(
        &self,
        actor: AuthenticatedActor,
        credential_id: CredentialId,
    ) -> Result<(), AuthError> {
        let context = actor.actor();
        let credential_id = credential_id.as_uuid();
        let mut transaction = self
            .store
            .begin_serializable()
            .await
            .map_err(|_| AuthError::Durable)?;
        self.authority
            .verify_fence_in_transaction(&mut transaction)
            .await
            .map_err(|_| AuthError::Durable)?;
        crate::authorization::verify_current_actor(&mut transaction, context)
            .await
            .map_err(|_| AuthError::CredentialInvalid)?;
        #[cfg(test)]
        self.pause_revoke_before_update().await?;
        sqlx::query_scalar::<_, Uuid>(
            "SELECT credential_id FROM relay_credentials WHERE credential_id = $1 AND principal_id = $2 FOR UPDATE",
        )
        .bind(credential_id)
        .bind(context.principal_id())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?
        .ok_or(AuthError::CredentialInvalid)?;
        crate::authorization::verify_current_actor(&mut transaction, context)
            .await
            .map_err(|_| AuthError::CredentialInvalid)?;
        let changed = sqlx::query(
            "UPDATE relay_credentials c SET revoked_at = clock_timestamp(), credential_generation = credential_generation + 1 \
             FROM relay_identity r WHERE c.credential_id = $1 AND c.principal_id = $2 \
             AND c.recovery_generation = r.recovery_generation AND r.state = 'normal' \
             AND c.revoked_at IS NULL AND c.expires_at > clock_timestamp() \
             AND (c.rotation_overlap_ends_at IS NULL OR c.rotation_overlap_ends_at > clock_timestamp()) \
             RETURNING c.credential_id",
        )
        .bind(credential_id)
        .bind(context.principal_id())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?;
        if changed.is_none() {
            let inactive = sqlx::query_scalar::<_, Uuid>(
                "SELECT c.credential_id FROM relay_credentials c JOIN relay_identity r ON r.state = 'normal' \
                 WHERE c.credential_id = $1 AND c.principal_id = $2 \
                 AND (c.revoked_at IS NOT NULL OR c.expires_at <= clock_timestamp() \
                      OR c.recovery_generation <> r.recovery_generation \
                      OR c.rotation_overlap_ends_at <= clock_timestamp()) FOR SHARE",
            )
            .bind(credential_id)
            .bind(context.principal_id())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(database_error)?;
            if inactive.is_some() {
                return self.commit_revoke_retry(transaction, context).await;
            }
            return Err(AuthError::CredentialInvalid);
        }
        audit_actor_mutation(
            &mut transaction,
            context,
            "auth.credential.revoke",
            "credential",
            database_error,
        )
        .await?;
        self.authority
            .commit_auth_mutation(
                transaction,
                true,
                Some(AuthMutationTarget::Credential(credential_id)),
            )
            .await
            .map_err(|_| AuthError::Durable)
    }

    /// Revokes a service credential after a current team-administrator check.
    pub async fn revoke_service_credential(
        &self,
        actor: AuthenticatedActor,
        team_id: TeamId,
        principal_id: PrincipalId,
        credential_id: CredentialId,
    ) -> Result<(), AuthError> {
        let context = actor.actor();
        let team_id = team_id.as_uuid();
        let principal_id = principal_id.as_uuid();
        let credential_id = credential_id.as_uuid();
        let mut transaction = self
            .store
            .begin_serializable()
            .await
            .map_err(|_| AuthError::Durable)?;
        self.authority
            .verify_fence_in_transaction(&mut transaction)
            .await
            .map_err(|_| AuthError::Durable)?;
        crate::authorization::verify_current_actor(&mut transaction, context)
            .await
            .map_err(|_| AuthError::CredentialInvalid)?;
        crate::authorization::rbac::require_team_admin(&mut transaction, context, team_id)
            .await
            .map_err(|_| AuthError::CredentialInvalid)?;
        #[cfg(test)]
        self.pause_revoke_before_update().await?;
        sqlx::query_scalar::<_, Uuid>(
            "SELECT c.credential_id FROM relay_credentials c JOIN service_accounts s ON s.principal_id = c.principal_id \
             WHERE c.credential_id = $1 AND c.principal_id = $2 AND s.team_id = $3 \
             AND c.credential_kind = 'service' FOR UPDATE",
        )
        .bind(credential_id)
        .bind(principal_id)
        .bind(team_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?
        .ok_or(AuthError::CredentialInvalid)?;
        crate::authorization::verify_current_actor(&mut transaction, context)
            .await
            .map_err(|_| AuthError::CredentialInvalid)?;
        let changed = sqlx::query(
            "UPDATE relay_credentials c SET revoked_at = clock_timestamp(), credential_generation = credential_generation + 1 \
             FROM service_accounts s JOIN relay_identity r ON r.state = 'normal' \
             WHERE c.credential_id = $1 AND c.principal_id = s.principal_id \
             AND s.team_id = $2 AND s.principal_id = $3 AND c.credential_kind = 'service' \
             AND c.recovery_generation = r.recovery_generation \
             AND c.revoked_at IS NULL AND c.expires_at > clock_timestamp() \
             AND (c.rotation_overlap_ends_at IS NULL OR c.rotation_overlap_ends_at > clock_timestamp()) \
             RETURNING c.credential_id",
        ).bind(credential_id).bind(team_id).bind(principal_id).fetch_optional(&mut *transaction).await.map_err(database_error)?;
        if changed.is_none() {
            let inactive = sqlx::query_scalar::<_, Uuid>(
                "SELECT c.credential_id FROM relay_credentials c \
                 JOIN service_accounts s ON s.principal_id = c.principal_id \
                 JOIN relay_identity r ON r.state = 'normal' \
                 WHERE c.credential_id = $1 AND s.team_id = $2 AND s.principal_id = $3 \
                 AND c.credential_kind = 'service' \
                 AND (c.revoked_at IS NOT NULL OR c.expires_at <= clock_timestamp() \
                      OR c.recovery_generation <> r.recovery_generation \
                      OR c.rotation_overlap_ends_at <= clock_timestamp()) FOR SHARE",
            )
            .bind(credential_id)
            .bind(team_id)
            .bind(principal_id)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(database_error)?;
            if inactive.is_some() {
                return self.commit_revoke_retry(transaction, context).await;
            }
            return Err(AuthError::CredentialInvalid);
        }
        audit_actor_mutation(
            &mut transaction,
            context,
            "auth.service_credential.revoke",
            "credential",
            database_error,
        )
        .await?;
        self.authority
            .commit_auth_mutation(
                transaction,
                true,
                Some(AuthMutationTarget::Credential(credential_id)),
            )
            .await
            .map_err(|_| AuthError::Durable)
    }

    /// Replaces a current account credential with a bounded overlap.
    pub async fn rotate_credential(
        &self,
        actor: AuthenticatedActor,
        credential_id: CredentialId,
        request: RotateCredentialRequest,
    ) -> Result<CredentialMutation, AuthError> {
        self.rotate_credential_for(
            actor,
            actor.actor().principal_id(),
            None,
            credential_id,
            request,
        )
        .await
    }

    /// Rotates a service credential after a current team-administrator check.
    pub async fn rotate_service_credential(
        &self,
        actor: AuthenticatedActor,
        team_id: TeamId,
        principal_id: PrincipalId,
        credential_id: CredentialId,
        request: RotateCredentialRequest,
    ) -> Result<CredentialMutation, AuthError> {
        self.rotate_credential_for(
            actor,
            principal_id.as_uuid(),
            Some(team_id.as_uuid()),
            credential_id,
            request,
        )
        .await
    }

    async fn rotate_credential_for(
        &self,
        actor: AuthenticatedActor,
        target_principal_id: Uuid,
        team_id: Option<Uuid>,
        credential_id: CredentialId,
        request: RotateCredentialRequest,
    ) -> Result<CredentialMutation, AuthError> {
        for _attempt in 0..IDENTITY_ISSUE_MAX_ATTEMPTS {
            match self
                .rotate_credential_for_once(
                    actor,
                    target_principal_id,
                    team_id,
                    credential_id,
                    request.clone(),
                )
                .await
            {
                Err(AuthError::Retryable) => {
                    #[cfg(test)]
                    self.rotation_retry_count
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                result => return result,
            }
        }
        Err(AuthError::Durable)
    }

    async fn rotate_credential_for_once(
        &self,
        actor: AuthenticatedActor,
        target_principal_id: Uuid,
        team_id: Option<Uuid>,
        credential_id: CredentialId,
        request: RotateCredentialRequest,
    ) -> Result<CredentialMutation, AuthError> {
        let context = actor.actor();
        if team_id.is_none() && context.kind() == ActorKind::Service {
            return Err(AuthError::CredentialInvalid);
        }
        let old_id = credential_id.as_uuid();
        let overlap = Duration::from_secs(u64::from(request.overlap_seconds));
        let secret = random_secret()?;
        let new_id = Uuid::now_v7();
        let public_id = new_id.to_string();
        let mut transaction = self
            .store
            .begin_serializable()
            .await
            .map_err(|_| AuthError::Durable)?;
        self.authority
            .verify_fence_in_transaction(&mut transaction)
            .await
            .map_err(|_| AuthError::Durable)?;
        crate::authorization::verify_current_actor(&mut transaction, context)
            .await
            .map_err(rotation_store_error)?;
        if let Some(team_id) = team_id {
            crate::authorization::rbac::require_team_admin(&mut transaction, context, team_id)
                .await
                .map_err(rotation_store_error)?;
            let service_account = sqlx::query_scalar::<_, Uuid>(
                "SELECT principal_id FROM service_accounts WHERE principal_id = $1 AND team_id = $2 AND deprovisioned_at IS NULL FOR SHARE",
            )
            .bind(target_principal_id)
            .bind(team_id)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(retryable_database_error)?;
            if service_account.is_none() {
                return Err(AuthError::CredentialInvalid);
            }
        }
        let request_digest = rotation_request_digest(&self.digest_key, old_id, &request)?;
        if let Some(result_id) = exact_receipt(
            &mut transaction,
            context,
            "auth.credential.rotate",
            request.idempotency.idempotency_key,
            &request_digest,
        )
        .await?
        {
            let row =
                credential_row_for_owner(&mut transaction, result_id, target_principal_id).await?;
            self.authority
                .commit_auth_mutation(transaction, false, None)
                .await
                .map_err(retryable_rotation_authority_error)?;
            return Ok(CredentialMutation {
                record: credential_record(row)?,
                credential: None,
            });
        }
        #[cfg(test)]
        self.pause_rotation_after_receipt().await?;
        if request.expires_at.nanosecond() % 1_000 != 0 {
            return Err(AuthError::Malformed);
        }
        let old = sqlx::query(
            "SELECT credential_kind, identity_id, credential_generation, rotation_family_id, recovery_generation, \
                    revoked_at, expires_at, rotation_overlap_ends_at, clock_timestamp() AS evaluated_at \
             FROM relay_credentials c WHERE c.credential_id = $1 AND c.principal_id = $2 FOR UPDATE",
        )
        .bind(old_id)
        .bind(target_principal_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(retryable_database_error)?
        .ok_or(AuthError::CredentialInvalid)?;
        let old_expires_at: time::OffsetDateTime = old.get("expires_at");
        let old_overlap_ends_at: Option<time::OffsetDateTime> = old.get("rotation_overlap_ends_at");
        let evaluated_at: time::OffsetDateTime = old.get("evaluated_at");
        let old_recovery_generation: i64 = old.get("recovery_generation");
        let old_revoked_at: Option<time::OffsetDateTime> = old.get("revoked_at");
        let requested_overlap =
            time::Duration::try_from(overlap).map_err(|_| AuthError::Malformed)?;
        if old_revoked_at.is_some()
            || old_recovery_generation != context.recovery_generation()
            || old_expires_at <= evaluated_at
            || old_overlap_ends_at.is_some()
        {
            self.commit_rotation_rejection(transaction, context).await?;
            return Err(AuthError::RotationRejected);
        }
        if request.expires_at <= evaluated_at {
            self.commit_rotation_rejection(transaction, context).await?;
            return Err(AuthError::RotationExpired);
        }
        if overlap > self.limits.max_rotation_overlap {
            self.commit_rotation_rejection(transaction, context).await?;
            return Err(AuthError::RotationRejected);
        }
        let kind: String = old.get("credential_kind");
        let lifetime = if kind == "service" {
            self.limits.service_credential_lifetime
        } else {
            self.limits.human_credential_lifetime
        };
        if request.expires_at
            > evaluated_at + time::Duration::try_from(lifetime).map_err(|_| AuthError::Malformed)?
        {
            self.commit_rotation_rejection(transaction, context).await?;
            return Err(AuthError::RotationRejected);
        }
        if old_expires_at < evaluated_at + requested_overlap {
            self.commit_rotation_rejection(transaction, context).await?;
            return Err(AuthError::RotationRejected);
        }
        let updated = sqlx::query(
            "UPDATE relay_credentials c SET rotation_overlap_ends_at = statement_timestamp() + $2::interval, \
             credential_generation = c.credential_generation + 1 \
             FROM relay_identity r WHERE c.credential_id = $1 AND c.revoked_at IS NULL \
             AND r.state = 'normal' AND c.recovery_generation = r.recovery_generation \
             AND $3::timestamptz > statement_timestamp() \
             AND c.expires_at >= statement_timestamp() + $2::interval \
             AND c.rotation_overlap_ends_at IS NULL",
        )
        .bind(old_id)
        .bind(interval(overlap))
        .bind(request.expires_at)
        .execute(&mut *transaction)
        .await
        .map_err(retryable_database_error)?;
        if updated.rows_affected() != 1 {
            let current_time: time::OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
                .fetch_one(&mut *transaction)
                .await
                .map_err(retryable_database_error)?;
            self.commit_rotation_rejection(transaction, context).await?;
            if request.expires_at <= current_time {
                return Err(AuthError::RotationExpired);
            }
            if old_expires_at < current_time + requested_overlap {
                return Err(AuthError::RotationRejected);
            }
            return Err(AuthError::CredentialInvalid);
        }
        enforce_credential_capacity(
            &mut transaction,
            target_principal_id,
            self.limits.credentials_per_principal,
            false,
        )
        .await?;
        let row = sqlx::query(
            "INSERT INTO relay_credentials (credential_id, public_id, secret_digest, principal_id, identity_id, digest_key_id, credential_kind, credential_generation, rotation_family_id, predecessor_credential_id, recovery_generation, expires_at) \
             SELECT $1, $2, $3, c.principal_id, c.identity_id, $4, c.credential_kind, 1, COALESCE(c.rotation_family_id, c.credential_id), c.credential_id, c.recovery_generation, $5 \
             FROM relay_credentials c WHERE c.credential_id = $6 \
             RETURNING credential_id, principal_id, credential_kind, issued_at, expires_at, rotation_overlap_ends_at, last_used_at, revoked_at",
        )
        .bind(new_id)
        .bind(&public_id)
        .bind(self.digest_key.digest(&secret).as_slice())
        .bind(self.digest_key.key_id())
        .bind(request.expires_at)
        .bind(old_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(retryable_database_error)?;
        let audit_id = audit_actor_mutation(
            &mut transaction,
            context,
            "auth.credential.rotate",
            "credential",
            retryable_database_error,
        )
        .await?;
        insert_receipt(
            &mut transaction,
            context,
            "auth.credential.rotate",
            request.idempotency.idempotency_key,
            &request_digest,
            audit_id,
            new_id,
        )
        .await?;
        self.authority
            .commit_auth_mutation(
                transaction,
                true,
                Some(AuthMutationTarget::Credential(old_id)),
            )
            .await
            .map_err(|_| AuthError::Durable)?;
        Ok(CredentialMutation {
            record: credential_record(row)?,
            credential: Some(DeviceCredential {
                credential_id: CredentialId::from_uuid(new_id),
                secret: Secret::new(secret),
                expires_at: request.expires_at,
            }),
        })
    }

    /// Creates a team-scoped service principal without assigning grants.
    pub async fn create_service_account(
        &self,
        actor: AuthenticatedActor,
        team_id: TeamId,
        request: CreateServiceAccountRequest,
    ) -> Result<ServiceAccountCreated, AuthError> {
        const MAX_SERVICE_ACCOUNT_NAME_BYTES: usize = 256;
        let context = actor.actor();
        let team_id = team_id.as_uuid();
        if request.display_name.is_empty()
            || request.display_name.len() > MAX_SERVICE_ACCOUNT_NAME_BYTES
            || request.expires_at <= time::OffsetDateTime::now_utc()
            || request.expires_at.nanosecond() % 1_000 != 0
            || request.expires_at
                > time::OffsetDateTime::now_utc() + self.limits.service_credential_lifetime
        {
            return Err(AuthError::Malformed);
        }
        let secret = random_secret()?;
        let principal_id = Uuid::now_v7();
        let credential_id = Uuid::now_v7();
        let mut transaction = self
            .store
            .begin_serializable()
            .await
            .map_err(|_| AuthError::Durable)?;
        self.authority
            .verify_fence_in_transaction(&mut transaction)
            .await
            .map_err(|_| AuthError::Durable)?;
        crate::authorization::verify_current_actor(&mut transaction, context)
            .await
            .map_err(|_| AuthError::CredentialInvalid)?;
        crate::authorization::rbac::require_team_admin(&mut transaction, context, team_id)
            .await
            .map_err(|_| AuthError::CredentialInvalid)?;
        let request_digest = request_digest(&self.digest_key, &(team_id, &request))?;
        if let Some(principal_id) = exact_receipt(
            &mut transaction,
            context,
            "auth.service_account.create",
            request.idempotency.idempotency_key,
            &request_digest,
        )
        .await?
        {
            let replay =
                service_account_created_replay(&mut transaction, principal_id, team_id).await?;
            self.authority
                .commit_auth_mutation(transaction, false, None)
                .await
                .map_err(|_| AuthError::Durable)?;
            return Ok(replay);
        }
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM service_accounts WHERE team_id = $1 AND deprovisioned_at IS NULL",
        )
        .bind(team_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(database_error)?;
        if usize::try_from(count).map_err(|_| AuthError::Durable)?
            >= self.limits.service_accounts_per_team
        {
            return Err(AuthError::Capacity);
        }
        sqlx::query("INSERT INTO principals (id, kind, state, generation) VALUES ($1, 'service', 'active', 1)")
            .bind(principal_id).execute(&mut *transaction).await.map_err(database_error)?;
        sqlx::query("INSERT INTO service_accounts (principal_id, team_id, display_name) VALUES ($1, $2, $3)")
            .bind(principal_id).bind(team_id).bind(&request.display_name).execute(&mut *transaction).await.map_err(database_error)?;
        // This membership carries the team relationship only.  No grants or
        // custom roles are created for a new service account.
        sqlx::query("INSERT INTO memberships (membership_id, team_id, principal_id, builtin_role, state, revision, local_deny_generation) VALUES ($1, $2, $3, 'member', 'active', 1, 1)")
            .bind(Uuid::now_v7()).bind(team_id).bind(principal_id).execute(&mut *transaction).await.map_err(database_error)?;
        let row = sqlx::query(
            "INSERT INTO relay_credentials (credential_id, public_id, secret_digest, principal_id, digest_key_id, credential_kind, credential_generation, rotation_family_id, recovery_generation, expires_at) \
             SELECT $1, $2, $3, $4, $5, 'service', 1, $1, r.recovery_generation, $6 FROM relay_identity r \
             WHERE r.state = 'normal' AND r.recovery_generation = $7 \
             RETURNING credential_id, principal_id, credential_kind, issued_at, expires_at, rotation_overlap_ends_at, last_used_at, revoked_at",
        ).bind(credential_id).bind(credential_id.to_string()).bind(self.digest_key.digest(&secret).as_slice())
            .bind(principal_id).bind(self.digest_key.key_id()).bind(request.expires_at).bind(context.recovery_generation())
            .fetch_one(&mut *transaction).await.map_err(database_error)?;
        let audit_id = audit_actor_mutation(
            &mut transaction,
            context,
            "auth.service_account.create",
            "service_account",
            database_error,
        )
        .await?;
        insert_receipt(
            &mut transaction,
            context,
            "auth.service_account.create",
            request.idempotency.idempotency_key,
            &request_digest,
            audit_id,
            principal_id,
        )
        .await?;
        self.authority
            .commit_auth_mutation(
                transaction,
                true,
                Some(AuthMutationTarget::Principal(principal_id)),
            )
            .await
            .map_err(|_| AuthError::Durable)?;
        Ok(ServiceAccountCreated {
            account: ServiceAccountRecord {
                principal_id: PrincipalId::from_uuid(principal_id),
                team_id: TeamId::from_uuid(team_id),
                display_name: request.display_name,
                state: PrincipalState::Active,
            },
            issued: CredentialMutation {
                record: credential_record(row)?,
                credential: Some(DeviceCredential {
                    credential_id: CredentialId::from_uuid(credential_id),
                    secret: Secret::new(secret),
                    expires_at: request.expires_at,
                }),
            },
        })
    }

    /// Lists a team's service account metadata for an authorized team administrator.
    pub async fn list_service_accounts(
        &self,
        actor: AuthenticatedActor,
        team_id: TeamId,
        page: PageRequest,
    ) -> Result<ServiceAccountPage, AuthError> {
        let limit = page_limit(page.limit)?;
        let context = actor.actor();
        let team_id = team_id.as_uuid();
        let mut transaction = self
            .store
            .begin_serializable()
            .await
            .map_err(|_| AuthError::Durable)?;
        self.authority
            .verify_fence_in_transaction(&mut transaction)
            .await
            .map_err(|_| AuthError::Durable)?;
        crate::authorization::rbac::require_team_admin(&mut transaction, context, team_id)
            .await
            .map_err(|_| AuthError::CredentialInvalid)?;
        let mut records = sqlx::query(
            "SELECT s.principal_id, s.team_id, s.display_name, p.state FROM service_accounts s \
             JOIN principals p ON p.id = s.principal_id WHERE s.team_id = $1 AND ($2::uuid IS NULL OR s.principal_id > $2) ORDER BY s.principal_id ASC LIMIT $3",
        ).bind(team_id).bind(page.after).bind(limit + 1).fetch_all(&mut *transaction).await.map_err(database_error)?
            .into_iter().map(|row| Ok(ServiceAccountRecord {
                principal_id: PrincipalId::from_uuid(row.get("principal_id")), team_id: TeamId::from_uuid(row.get("team_id")),
                display_name: row.get("display_name"), state: principal_state(row.get("state"))?,
            })).collect::<Result<Vec<_>, AuthError>>()?;
        let next_cursor = service_account_cursor(&mut records, limit);
        self.commit_current(transaction).await?;
        Ok(ServiceAccountPage {
            records,
            next_cursor,
        })
    }

    async fn commit_current(
        &self,
        mut transaction: Transaction<'_, Postgres>,
    ) -> Result<(), AuthError> {
        self.authority
            .verify_fence_in_transaction(&mut transaction)
            .await
            .map_err(|_| AuthError::Durable)?;
        transaction.commit().await.map_err(|_| AuthError::Durable)
    }

    async fn commit_rotation_rejection(
        &self,
        mut transaction: Transaction<'_, Postgres>,
        context: crate::store::ActorContext,
    ) -> Result<(), AuthError> {
        crate::authorization::verify_current_actor(&mut transaction, context)
            .await
            .map_err(rotation_store_error)?;
        self.authority
            .verify_fence_in_transaction(&mut transaction)
            .await
            .map_err(|_| AuthError::Durable)?;
        transaction.commit().await.map_err(retryable_database_error)
    }

    async fn commit_revoke_retry(
        &self,
        mut transaction: Transaction<'_, Postgres>,
        context: crate::store::ActorContext,
    ) -> Result<(), AuthError> {
        crate::authorization::verify_current_actor(&mut transaction, context)
            .await
            .map_err(|_| AuthError::CredentialInvalid)?;
        self.authority
            .commit_auth_mutation(transaction, false, None)
            .await
            .map_err(|_| AuthError::Durable)
    }

    #[cfg(test)]
    async fn pause_rotation_after_receipt(&self) -> Result<(), AuthError> {
        let hook = self.rotation_hook.lock().await.clone();
        if let Some(hook) = hook {
            hook.entered.notify_one();
            hook.release.notified().await;
        }
        Ok(())
    }

    #[cfg(test)]
    async fn pause_revoke_before_update(&self) -> Result<(), AuthError> {
        let hook = self.revoke_hook.lock().await.clone();
        if let Some(hook) = hook {
            hook.entered.notify_one();
            hook.release.notified().await;
        }
        Ok(())
    }

    async fn remove_pending_browser_login(&self, login_id: Uuid) -> Option<PendingBrowserLogin> {
        self.pending_browser_logins.lock().await.remove(&login_id)
    }

    /// Claims a device code after proving the polling possession secret.
    ///
    /// # Errors
    /// Returns [`AuthError::CredentialInvalid`] for an invalid polling secret
    /// and [`AuthError::DeviceExpired`] after restart or terminal consumption.
    #[expect(
        dead_code,
        reason = "The server polling route calls this auth boundary."
    )]
    pub(crate) async fn take_device_code(
        &self,
        login_id: Uuid,
        poll_secret: DevicePollSecret,
        stored_digest: &[u8],
    ) -> Result<PendingDeviceCode, AuthError> {
        if !self.digest_key.matches(poll_secret.expose(), stored_digest) {
            return Err(AuthError::CredentialInvalid);
        }
        self.pending_device_codes
            .take(login_id)
            .await
            .ok_or(AuthError::DeviceExpired)
    }

    /// Restores a code after an RFC 8628 pending response.
    #[expect(
        dead_code,
        reason = "The server polling route restores nonterminal provider responses."
    )]
    pub(crate) async fn restore_device_code(&self, login_id: Uuid, device_code: PendingDeviceCode) {
        self.pending_device_codes
            .restore(login_id, device_code)
            .await;
    }

    /// Clears every transient device code during shutdown or quarantine.
    #[expect(
        dead_code,
        reason = "Shutdown and recovery quarantine call this lifecycle hook."
    )]
    pub(crate) async fn clear_pending_device_codes(&self) {
        self.pending_device_codes.clear().await;
    }

    async fn prune_expired_pending(&self) -> Result<(), AuthError> {
        let mut transaction = self
            .store
            .begin_serializable()
            .await
            .map_err(|_| AuthError::Durable)?;
        self.authority
            .verify_fence_in_transaction(&mut transaction)
            .await
            .map_err(|_| AuthError::Durable)?;
        sqlx::query(
            "UPDATE browser_logins SET consumed_at = clock_timestamp(), outcome = 'expired' \
             WHERE consumed_at IS NULL AND expires_at <= clock_timestamp()",
        )
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        sqlx::query(
            "UPDATE device_logins SET consumed_at = clock_timestamp(), outcome = 'expired', poll_lease_until = NULL, poll_lease_token = NULL \
             WHERE consumed_at IS NULL AND expires_at <= clock_timestamp()",
        )
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        self.commit_current(transaction).await?;
        Ok(())
    }

    async fn reject_device_login(
        &self,
        login_id: Uuid,
        outcome: &'static str,
        poll_lease_token: Option<&[u8; RANDOM_SECRET_BYTES]>,
    ) -> Result<(), AuthError> {
        let mut transaction = self
            .store
            .begin_serializable()
            .await
            .map_err(|_| AuthError::Durable)?;
        self.authority
            .verify_fence_in_transaction(&mut transaction)
            .await
            .map_err(|_| AuthError::Durable)?;
        let updated = sqlx::query(
            "UPDATE device_logins SET consumed_at = clock_timestamp(), outcome = $2, poll_lease_until = NULL, poll_lease_token = NULL \
             WHERE login_id = $1 AND consumed_at IS NULL AND ($3::bytea IS NULL OR (poll_lease_token = $3 AND expires_at > clock_timestamp()))",
        )
        .bind(login_id)
        .bind(outcome)
        .bind(poll_lease_token.map(|token| token.as_slice()))
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        if updated.rows_affected() != 1 {
            return Err(AuthError::DeviceExpired);
        }
        audit_auth(
            &mut transaction,
            "auth.device.poll",
            "deny",
            "device_login",
            outcome,
            None,
        )
        .await?;
        self.commit_current(transaction).await?;
        Ok(())
    }

    async fn audit_rejection(
        &self,
        action: &'static str,
        parameter_value: &'static str,
        outcome: &'static str,
    ) -> Result<(), AuthError> {
        let mut transaction = self
            .store
            .begin_serializable()
            .await
            .map_err(|_| AuthError::Durable)?;
        self.authority
            .verify_fence_in_transaction(&mut transaction)
            .await
            .map_err(|_| AuthError::Durable)?;
        audit_auth(
            &mut transaction,
            action,
            "deny",
            parameter_value,
            outcome,
            None,
        )
        .await?;
        self.commit_current(transaction).await
    }

    /// Returns the current HMAC key identifier for durable digest rows.
    #[must_use]
    #[expect(
        dead_code,
        reason = "Durable auth rows record the active digest-key coordinate."
    )]
    pub(crate) fn digest_key_id(&self) -> &str {
        self.digest_key.key_id()
    }

    async fn issue_browser_session(
        &self,
        issuer: String,
        subject: String,
        client_id: String,
        redirect_uri: String,
        login_id: Uuid,
        recovery_generation: i64,
    ) -> Result<IssuedBrowserSession, AuthError> {
        for _attempt in 0..IDENTITY_ISSUE_MAX_ATTEMPTS {
            match self
                .issue_browser_session_once(
                    issuer.clone(),
                    subject.clone(),
                    client_id.clone(),
                    redirect_uri.clone(),
                    login_id,
                    recovery_generation,
                )
                .await
            {
                Err(AuthError::Retryable) => continue,
                result => return result,
            }
        }
        Err(AuthError::Durable)
    }

    async fn issue_browser_session_once(
        &self,
        issuer: String,
        subject: String,
        client_id: String,
        redirect_uri: String,
        login_id: Uuid,
        recovery_generation: i64,
    ) -> Result<IssuedBrowserSession, AuthError> {
        let cookie = random_secret()?;
        let csrf = random_secret()?;
        let session_id = Uuid::now_v7();
        let mut transaction = self
            .store
            .begin_serializable()
            .await
            .map_err(|_| AuthError::Durable)?;
        self.authority
            .verify_fence_in_transaction(&mut transaction)
            .await
            .map_err(|_| AuthError::Durable)?;
        let bound = sqlx::query(
            "SELECT b.login_id FROM browser_logins b JOIN relay_identity r ON r.recovery_generation = b.recovery_generation \
             WHERE b.login_id = $1 AND b.issuer = $2 AND b.client_id = $3 AND b.audience = $3 AND b.redirect_uri = $4 \
             AND b.action = 'login' AND b.link_id IS NULL AND b.account_link_generation = 1 AND b.consumed_at IS NOT NULL \
             AND b.outcome = 'failed' AND b.expires_at > clock_timestamp() AND b.recovery_generation = $5 AND r.state = 'normal' FOR UPDATE",
        )
        .bind(login_id)
        .bind(&issuer)
        .bind(&client_id)
        .bind(&redirect_uri)
        .bind(recovery_generation)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(retryable_database_error)?;
        if bound.is_none() {
            return Err(AuthError::OidcInvalid);
        }
        if !self.subject_is_allowed(&subject) {
            audit_auth(
                &mut transaction,
                "auth.browser.session.issue",
                "deny",
                "oidc_subject",
                "policy_denied",
                Some(recovery_generation),
            )
            .await?;
            self.commit_current(transaction).await?;
            return Err(AuthError::CredentialInvalid);
        }
        let identity = canonical_human_principal(&mut transaction, &issuer, &subject).await?;
        let expires_at = sqlx::query(
            "INSERT INTO browser_sessions (session_id, cookie_digest, csrf_digest, principal_id, identity_id, digest_key_id, session_generation, recovery_generation, expires_at, idle_deadline) \
             SELECT $1, $2, $3, $4, $5, $6, 1, r.recovery_generation, clock_timestamp() + $7::interval, clock_timestamp() + $8::interval \
             FROM relay_identity r WHERE r.state = 'normal' AND r.recovery_generation = $9 RETURNING expires_at",
        ).bind(session_id).bind(self.digest_key.digest(&cookie).as_slice()).bind(self.digest_key.digest(&csrf).as_slice()).bind(identity.principal_id).bind(identity.identity_id).bind(self.digest_key.key_id())
        .bind(interval(self.limits.browser_session_lifetime)).bind(interval(self.limits.browser_session_idle)).bind(recovery_generation)
        .fetch_optional(&mut *transaction).await.map_err(database_error)?.ok_or(AuthError::OidcInvalid)?
        .get("expires_at");
        let updated = sqlx::query("UPDATE browser_logins SET outcome = 'succeeded' WHERE login_id = $1 AND outcome = 'failed' AND consumed_at IS NOT NULL AND expires_at > clock_timestamp()")
            .bind(login_id)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
        if updated.rows_affected() != 1 {
            return Err(AuthError::OidcInvalid);
        }
        audit_auth(
            &mut transaction,
            "auth.browser.session.issue",
            "allow",
            "browser_session",
            "committed",
            Some(recovery_generation),
        )
        .await?;
        self.commit_current(transaction).await?;
        Ok(IssuedBrowserSession {
            session_id,
            cookie: super::SecretValue::new(cookie),
            csrf: super::SecretValue::new(csrf),
            expires_at,
        })
    }

    async fn issue_human_credential(
        &self,
        subject: String,
        binding: DeviceIssueBinding,
    ) -> Result<super::IssuedCredential, AuthError> {
        for _attempt in 0..IDENTITY_ISSUE_MAX_ATTEMPTS {
            match self
                .issue_human_credential_once(subject.clone(), &binding)
                .await
            {
                Err(AuthError::Retryable) => continue,
                result => return result,
            }
        }
        Err(AuthError::Durable)
    }

    async fn issue_human_credential_once(
        &self,
        subject: String,
        binding: &DeviceIssueBinding,
    ) -> Result<super::IssuedCredential, AuthError> {
        let secret = random_secret()?;
        let credential_id = Uuid::now_v7();
        let public_id = credential_id.to_string();
        let mut transaction = self
            .store
            .begin_serializable()
            .await
            .map_err(|_| AuthError::Durable)?;
        self.authority
            .verify_fence_in_transaction(&mut transaction)
            .await
            .map_err(|_| AuthError::Durable)?;
        let bound = sqlx::query(
            "SELECT d.login_id, d.poll_interval_seconds FROM device_logins d JOIN relay_identity r ON r.recovery_generation = d.recovery_generation \
             WHERE d.login_id = $1 AND d.poll_lease_token = $2 AND d.poll_lease_until > clock_timestamp() \
             AND d.issuer = $3 AND d.client_id = $4 AND d.audience = $4 AND d.action = 'login' \
             AND d.link_id IS NULL AND d.account_link_generation = 1 AND d.recovery_generation = $5 \
             AND d.consumed_at IS NULL AND d.expires_at > clock_timestamp() AND r.state = 'normal' FOR UPDATE",
        )
        .bind(binding.login_id)
        .bind(binding.poll_lease_token.as_slice())
        .bind(&binding.issuer)
        .bind(&binding.client_id)
        .bind(binding.recovery_generation)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(retryable_database_error)?;
        let Some(bound) = bound else {
            return Err(AuthError::DeviceBusy {
                retry_after_seconds: binding.poll_interval_seconds,
            });
        };
        let retry_after_seconds = interval_seconds(bound.get("poll_interval_seconds"))?;
        if !self.subject_is_allowed(&subject) {
            audit_auth(
                &mut transaction,
                "auth.human_credential.issue",
                "deny",
                "oidc_subject",
                "policy_denied",
                Some(binding.recovery_generation),
            )
            .await?;
            self.commit_current(transaction).await?;
            return Err(AuthError::CredentialInvalid);
        }
        let identity =
            canonical_human_principal(&mut transaction, &binding.issuer, &subject).await?;
        enforce_credential_capacity(
            &mut transaction,
            identity.principal_id,
            self.limits.credentials_per_principal,
            false,
        )
        .await?;
        let expires_at = sqlx::query("INSERT INTO relay_credentials (credential_id, public_id, secret_digest, principal_id, identity_id, digest_key_id, credential_kind, credential_generation, rotation_family_id, recovery_generation, expires_at) SELECT $1, $2, $3, $4, $5, $6, 'human', 1, $1, r.recovery_generation, clock_timestamp() + $7::interval FROM relay_identity r WHERE r.state = 'normal' AND r.recovery_generation = $8 RETURNING expires_at")
            .bind(credential_id).bind(&public_id).bind(self.digest_key.digest(&secret).as_slice()).bind(identity.principal_id).bind(identity.identity_id).bind(self.digest_key.key_id()).bind(interval(self.limits.human_credential_lifetime)).bind(binding.recovery_generation).fetch_optional(&mut *transaction).await.map_err(database_error)?.ok_or(AuthError::DeviceExpired)?.get("expires_at");
        let consumed = sqlx::query("UPDATE device_logins SET consumed_at = clock_timestamp(), outcome = 'succeeded', poll_lease_until = NULL, poll_lease_token = NULL WHERE login_id = $1 AND poll_lease_token = $2 AND consumed_at IS NULL AND expires_at > clock_timestamp()")
            .bind(binding.login_id)
            .bind(binding.poll_lease_token.as_slice())
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
        if consumed.rows_affected() != 1 {
            return Err(AuthError::DeviceBusy {
                retry_after_seconds,
            });
        }
        audit_auth(
            &mut transaction,
            "auth.human_credential.issue",
            "allow",
            "human_credential",
            "committed",
            Some(binding.recovery_generation),
        )
        .await?;
        self.commit_current(transaction).await?;
        Ok(super::IssuedCredential::new(public_id, secret, expires_at))
    }

    fn subject_is_allowed(&self, subject: &str) -> bool {
        match &self.login_policy {
            LoginPolicy::AnyAuthenticatedSubject => true,
            LoginPolicy::AllowedSubjects(subjects) => {
                subjects.iter().any(|allowed| allowed == subject)
            }
        }
    }

    async fn touch_browser_session(&self, session_id: Uuid) -> Result<(), AuthError> {
        let mut transaction = self
            .store
            .begin_serializable()
            .await
            .map_err(|_| AuthError::Durable)?;
        self.authority
            .verify_fence_in_transaction(&mut transaction)
            .await
            .map_err(|_| AuthError::Durable)?;
        touch_browser_session(
            &mut transaction,
            session_id,
            self.limits.browser_session_idle,
        )
        .await?;
        self.commit_current(transaction).await
    }

    async fn touch_credential(&self, credential_id: Uuid) -> Result<(), AuthError> {
        let mut transaction = self
            .store
            .begin_serializable()
            .await
            .map_err(|_| AuthError::Durable)?;
        self.authority
            .verify_fence_in_transaction(&mut transaction)
            .await
            .map_err(|_| AuthError::Durable)?;
        touch_credential(&mut transaction, credential_id).await?;
        self.commit_current(transaction).await
    }
}

fn random_secret() -> Result<String, AuthError> {
    Ok(URL_SAFE_NO_PAD.encode(random_token()?))
}

fn random_token() -> Result<[u8; RANDOM_SECRET_BYTES], AuthError> {
    let mut bytes = [0_u8; RANDOM_SECRET_BYTES];
    getrandom::getrandom(&mut bytes).map_err(|_| AuthError::Durable)?;
    Ok(bytes)
}

fn interval_seconds(value: i32) -> Result<u32, AuthError> {
    u32::try_from(value).map_err(|_| AuthError::Durable)
}

fn browser_actor_kind(principal_kind: String) -> Result<ActorKind, AuthError> {
    match principal_kind.as_str() {
        "human" => Ok(ActorKind::Human),
        "infrastructure" => Ok(ActorKind::Infrastructure),
        _ => Err(AuthError::CredentialInvalid),
    }
}

fn credential_actor_kind(
    principal_kind: String,
    credential_kind: String,
) -> Result<ActorKind, AuthError> {
    match (principal_kind.as_str(), credential_kind.as_str()) {
        ("human", "human") => Ok(ActorKind::Human),
        ("infrastructure", "human") => Ok(ActorKind::Infrastructure),
        ("service", "service") => Ok(ActorKind::Service),
        _ => Err(AuthError::CredentialInvalid),
    }
}

fn principal_kind(value: String) -> Result<PrincipalKind, AuthError> {
    match value.as_str() {
        "human" => Ok(PrincipalKind::Human),
        "service" => Ok(PrincipalKind::Service),
        "infrastructure" => Ok(PrincipalKind::Infrastructure),
        _ => Err(AuthError::CredentialInvalid),
    }
}

fn principal_state(value: String) -> Result<PrincipalState, AuthError> {
    match value.as_str() {
        "active" => Ok(PrincipalState::Active),
        "deprovisioned" => Ok(PrincipalState::Deprovisioned),
        _ => Err(AuthError::CredentialInvalid),
    }
}

fn page_limit(limit: u16) -> Result<i64, AuthError> {
    const MAX_PAGE_SIZE: u16 = 128;
    if limit == 0 || limit > MAX_PAGE_SIZE {
        return Err(AuthError::Malformed);
    }
    Ok(i64::from(limit))
}

fn page_cursor(records: &mut Vec<CredentialRecord>, limit: i64) -> Option<Uuid> {
    if records.len() <= usize::try_from(limit).expect("page limit is positive") {
        return None;
    }
    records.pop();
    records.last().map(|record| record.credential_id.as_uuid())
}

fn service_account_cursor(records: &mut Vec<ServiceAccountRecord>, limit: i64) -> Option<Uuid> {
    if records.len() <= usize::try_from(limit).expect("page limit is positive") {
        return None;
    }
    records.pop();
    records.last().map(|record| record.principal_id.as_uuid())
}

fn credential_record(row: sqlx::postgres::PgRow) -> Result<CredentialRecord, AuthError> {
    let kind: String = row.get("credential_kind");
    let kind = match kind.as_str() {
        "human" => CredentialKind::Human,
        "service" => CredentialKind::Service,
        _ => return Err(AuthError::CredentialInvalid),
    };
    Ok(CredentialRecord {
        credential_id: CredentialId::from_uuid(row.get("credential_id")),
        principal_id: PrincipalId::from_uuid(row.get("principal_id")),
        kind,
        issued_at: row.get("issued_at"),
        expires_at: row.get("expires_at"),
        rotation_overlap_ends_at: row.get("rotation_overlap_ends_at"),
        last_used_at: row.get("last_used_at"),
        revoked_at: row.get("revoked_at"),
    })
}

async fn audit_actor_mutation(
    transaction: &mut Transaction<'_, Postgres>,
    actor: crate::store::ActorContext,
    action: &'static str,
    parameter_value: &'static str,
    map_error: fn(sqlx::Error) -> AuthError,
) -> Result<Uuid, AuthError> {
    let audit_id = Uuid::now_v7();
    let inserted = sqlx::query(
        "INSERT INTO audit_events (audit_id, actor_principal_id, actor_kind, team_id, action, decision, policy_generation, recovery_generation, correlation_id, parameter_code, parameter_value, outcome) \
         SELECT $1, $2, $3, NULL, $4, 'changed', 1, r.recovery_generation, $5, 'credential', $6, 'committed' \
         FROM relay_identity r WHERE r.state = 'normal' AND r.recovery_generation = $7",
    )
    .bind(audit_id)
    .bind(actor.principal_id())
    .bind(crate::store::actor_kind_name(actor.kind()))
    .bind(action)
    .bind(Uuid::now_v7())
    .bind(parameter_value)
    .bind(actor.recovery_generation())
    .execute(&mut **transaction)
    .await
    .map_err(map_error)?;
    if inserted.rows_affected() != 1 {
        return Err(AuthError::Durable);
    }
    Ok(audit_id)
}

fn request_digest<T: serde::Serialize>(
    key: &DigestKey,
    request: &T,
) -> Result<[u8; 32], AuthError> {
    let value = serde_json::to_string(request).map_err(|_| AuthError::Malformed)?;
    Ok(key.digest(&value))
}

fn rotation_request_digest(
    key: &DigestKey,
    credential_id: Uuid,
    request: &RotateCredentialRequest,
) -> Result<[u8; 32], AuthError> {
    request_digest(key, &(credential_id, request))
}

async fn exact_receipt(
    transaction: &mut Transaction<'_, Postgres>,
    actor: crate::store::ActorContext,
    action: &'static str,
    idempotency_key: Uuid,
    request_digest: &[u8; 32],
) -> Result<Option<Uuid>, AuthError> {
    let receipt = sqlx::query(
        "SELECT request_digest, result_id FROM mutation_receipts WHERE actor_principal_id = $1 \
         AND action = $2 AND idempotency_key = $3 FOR UPDATE",
    )
    .bind(actor.principal_id())
    .bind(action)
    .bind(idempotency_key)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(retryable_database_error)?;
    let Some(receipt) = receipt else {
        return Ok(None);
    };
    let stored: Vec<u8> = receipt.get("request_digest");
    if stored.as_slice() != request_digest {
        return Err(AuthError::IdempotencyConflict);
    }
    receipt
        .get::<Option<Uuid>, _>("result_id")
        .ok_or(AuthError::Durable)
        .map(Some)
}

async fn insert_receipt(
    transaction: &mut Transaction<'_, Postgres>,
    actor: crate::store::ActorContext,
    action: &'static str,
    idempotency_key: Uuid,
    request_digest: &[u8; 32],
    audit_id: Uuid,
    result_id: Uuid,
) -> Result<(), AuthError> {
    sqlx::query(
        "INSERT INTO mutation_receipts (actor_principal_id, action, idempotency_key, request_digest, audit_id, outcome_code, result_id, result_revision) \
         VALUES ($1, $2, $3, $4, $5, 'committed', $6, 1)",
    )
    .bind(actor.principal_id())
    .bind(action)
    .bind(idempotency_key)
    .bind(request_digest.as_slice())
    .bind(audit_id)
    .bind(result_id)
    .execute(&mut **transaction)
    .await
    .map_err(retryable_receipt_database_error)?;
    Ok(())
}

async fn credential_row_for_owner(
    transaction: &mut Transaction<'_, Postgres>,
    credential_id: Uuid,
    principal_id: Uuid,
) -> Result<sqlx::postgres::PgRow, AuthError> {
    sqlx::query(
        "SELECT credential_id, principal_id, credential_kind, issued_at, expires_at, rotation_overlap_ends_at, last_used_at, revoked_at \
         FROM relay_credentials WHERE credential_id = $1 AND principal_id = $2 FOR SHARE",
    )
    .bind(credential_id)
    .bind(principal_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(retryable_database_error)?
    .ok_or(AuthError::Durable)
}

async fn service_account_created_replay(
    transaction: &mut Transaction<'_, Postgres>,
    principal_id: Uuid,
    team_id: Uuid,
) -> Result<ServiceAccountCreated, AuthError> {
    let account = sqlx::query(
        "SELECT s.principal_id, s.team_id, s.display_name, p.state FROM service_accounts s \
         JOIN principals p ON p.id = s.principal_id WHERE s.principal_id = $1 AND s.team_id = $2 FOR SHARE",
    )
    .bind(principal_id)
    .bind(team_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(database_error)?
    .ok_or(AuthError::Durable)?;
    let credential = sqlx::query(
        "SELECT credential_id, principal_id, credential_kind, issued_at, expires_at, rotation_overlap_ends_at, last_used_at, revoked_at \
         FROM relay_credentials WHERE principal_id = $1 AND credential_kind = 'service' \
         ORDER BY issued_at ASC, credential_id ASC LIMIT 1 FOR SHARE",
    )
    .bind(principal_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(database_error)?
    .ok_or(AuthError::Durable)?;
    Ok(ServiceAccountCreated {
        account: ServiceAccountRecord {
            principal_id: PrincipalId::from_uuid(account.get("principal_id")),
            team_id: TeamId::from_uuid(account.get("team_id")),
            display_name: account.get("display_name"),
            state: principal_state(account.get("state"))?,
        },
        issued: CredentialMutation {
            record: credential_record(credential)?,
            credential: None,
        },
    })
}

async fn touch_browser_session(
    transaction: &mut Transaction<'_, Postgres>,
    session_id: Uuid,
    idle_lifetime: Duration,
) -> Result<(), AuthError> {
    let updated = sqlx::query(
        "UPDATE browser_sessions s SET last_used_at = clock_timestamp(), \
         idle_deadline = LEAST(s.expires_at, clock_timestamp() + $2::interval) \
         FROM principals p, relay_identity r \
         WHERE s.session_id = $1 AND s.principal_id = p.id AND p.state = 'active' \
         AND r.state = 'normal' AND s.recovery_generation = r.recovery_generation \
         AND s.revoked_at IS NULL AND s.expires_at > clock_timestamp() AND s.idle_deadline > clock_timestamp()",
    )
    .bind(session_id)
    .bind(interval(idle_lifetime))
    .execute(&mut **transaction)
    .await
    .map_err(retryable_database_error)?;
    if updated.rows_affected() != 1 {
        return Err(AuthError::CredentialInvalid);
    }
    Ok(())
}

async fn touch_credential(
    transaction: &mut Transaction<'_, Postgres>,
    credential_id: Uuid,
) -> Result<(), AuthError> {
    let updated = sqlx::query(
        "UPDATE relay_credentials c SET last_used_at = clock_timestamp() \
         FROM principals p, relay_identity r \
         WHERE c.credential_id = $1 AND c.principal_id = p.id AND p.state = 'active' \
         AND r.state = 'normal' AND c.recovery_generation = r.recovery_generation \
         AND c.revoked_at IS NULL AND c.expires_at > clock_timestamp() \
         AND (c.rotation_overlap_ends_at IS NULL OR c.rotation_overlap_ends_at > clock_timestamp()) \
         AND (c.credential_kind <> 'service' OR EXISTS (SELECT 1 FROM service_accounts s WHERE s.principal_id = c.principal_id AND s.deprovisioned_at IS NULL))",
    )
    .bind(credential_id)
    .execute(&mut **transaction)
    .await
    .map_err(retryable_database_error)?;
    if updated.rows_affected() != 1 {
        return Err(AuthError::CredentialInvalid);
    }
    Ok(())
}

fn interval(duration: Duration) -> String {
    format!("{} microseconds", duration.as_micros())
}

fn seconds_i32(duration: Duration) -> Result<i32, AuthError> {
    i32::try_from(duration.as_secs()).map_err(|_| AuthError::Malformed)
}

async fn audit_auth(
    transaction: &mut Transaction<'_, Postgres>,
    action: &'static str,
    decision: &'static str,
    parameter_value: &'static str,
    outcome: &'static str,
    expected_recovery_generation: Option<i64>,
) -> Result<(), AuthError> {
    let inserted = sqlx::query(
        "INSERT INTO audit_events (audit_id, actor_principal_id, actor_kind, team_id, action, decision, policy_generation, recovery_generation, correlation_id, parameter_code, parameter_value, outcome) \
         SELECT $1, NULL, 'system', NULL, $2, $3, 1, r.recovery_generation, $4, 'transaction', $5, $6 \
         FROM relay_identity r WHERE r.state = 'normal' AND ($7::bigint IS NULL OR r.recovery_generation = $7)",
    )
    .bind(Uuid::now_v7())
    .bind(action)
    .bind(decision)
    .bind(Uuid::now_v7())
    .bind(parameter_value)
    .bind(outcome)
    .bind(expected_recovery_generation)
    .execute(&mut **transaction)
    .await
    .map_err(|_| AuthError::Durable)?;
    if inserted.rows_affected() != 1 {
        return Err(AuthError::Durable);
    }
    Ok(())
}

async fn enforce_credential_capacity(
    transaction: &mut Transaction<'_, Postgres>,
    principal_id: Uuid,
    limit: usize,
    replacing_without_overlap: bool,
) -> Result<(), AuthError> {
    let locked = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM principals WHERE id = $1 AND state = 'active' FOR UPDATE",
    )
    .bind(principal_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(retryable_database_error)?;
    if locked.is_none() {
        return Err(AuthError::CredentialInvalid);
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM relay_credentials WHERE principal_id = $1 AND revoked_at IS NULL \
         AND expires_at > clock_timestamp() AND (rotation_overlap_ends_at IS NULL OR rotation_overlap_ends_at > clock_timestamp())",
    )
    .bind(principal_id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(retryable_database_error)?;
    let count = usize::try_from(count).map_err(|_| AuthError::Durable)?;
    if count >= limit && !replacing_without_overlap {
        return Err(AuthError::Capacity);
    }
    Ok(())
}

async fn canonical_human_principal(
    transaction: &mut Transaction<'_, Postgres>,
    issuer: &str,
    subject: &str,
) -> Result<HumanIdentity, AuthError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, $2))")
        .bind(format!("{issuer}\u{1f}{subject}"))
        .bind(IDENTITY_ADVISORY_LOCK_SEED)
        .execute(&mut **transaction)
        .await
        .map_err(retryable_database_error)?;
    let identity = sqlx::query(
        "SELECT principal_id, identity_id FROM oidc_identities WHERE issuer = $1 AND subject = $2 AND removed_at IS NULL FOR UPDATE",
    )
    .bind(issuer)
    .bind(subject)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(retryable_database_error)?;
    if let Some(row) = identity {
        return Ok(HumanIdentity {
            principal_id: row.get("principal_id"),
            identity_id: row.get("identity_id"),
        });
    }
    let principal_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO principals (id, kind, state, generation) VALUES ($1, 'human', 'active', 1)",
    )
    .bind(principal_id)
    .execute(&mut **transaction)
    .await
    .map_err(retryable_database_error)?;
    let identity_id = Uuid::now_v7();
    sqlx::query("INSERT INTO oidc_identities (identity_id, issuer, subject, principal_id, link_generation) VALUES ($1, $2, $3, $4, 1)")
        .bind(identity_id)
        .bind(issuer)
        .bind(subject)
        .bind(principal_id)
        .execute(&mut **transaction)
        .await
        .map_err(retryable_database_error)?;
    Ok(HumanIdentity {
        principal_id,
        identity_id,
    })
}

fn database_error(_error: sqlx::Error) -> AuthError {
    AuthError::Durable
}

fn retryable_database_error(error: sqlx::Error) -> AuthError {
    if error
        .as_database_error()
        .and_then(|database| database.code())
        .is_some_and(|code| code == "40001" || code == "40P01")
    {
        AuthError::Retryable
    } else {
        AuthError::Durable
    }
}

fn retryable_receipt_database_error(error: sqlx::Error) -> AuthError {
    let is_receipt_conflict = error.as_database_error().is_some_and(|database| {
        database.code().is_some_and(|code| code == "23505")
            && database.constraint() == Some("mutation_receipts_pkey")
    });
    if is_receipt_conflict {
        AuthError::Retryable
    } else {
        retryable_database_error(error)
    }
}

fn retryable_rotation_authority_error(error: AuthorityError) -> AuthError {
    match error {
        AuthorityError::Store(StoreError::Database(error)) => retryable_database_error(error),
        _ => AuthError::Durable,
    }
}

fn rotation_store_error(error: StoreError) -> AuthError {
    match error {
        StoreError::Forbidden => AuthError::CredentialInvalid,
        StoreError::Database(error) => retryable_database_error(error),
        _ => AuthError::Durable,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{env, os::unix::fs::PermissionsExt};

    use ed25519_dalek::SigningKey;
    use sqlx::{AssertSqlSafe, PgPool};

    use super::*;
    use crate::{
        admission::AuthorityLimits,
        recovery::WitnessStore,
        store::{AuthorizationRequest, ResourceScope},
    };

    const BOOTSTRAP_CONNECTIONS: u32 = 1;
    const TEST_CONNECTIONS: u32 = 4;

    pub(crate) fn limits() -> AuthLimits {
        AuthLimits::new(
            Duration::from_secs(300),
            Duration::from_secs(600),
            Duration::from_secs(60),
            Duration::from_secs(900),
            Duration::from_secs(900),
            Duration::from_secs(60),
            8,
            16,
            Duration::from_secs(30),
            Duration::from_secs(5),
            Duration::from_secs(60),
        )
        .expect("valid explicit authentication limits")
    }

    #[test]
    fn login_start_debug_never_exposes_browser_or_device_secrets() {
        let browser = BrowserLoginStart {
            login_id: Uuid::nil(),
            authorization_url: url::Url::parse(
                "https://issuer.example/auth?state=sentinel-state&nonce=sentinel-nonce",
            )
            .expect("valid fixture URL"),
        };
        let device = DeviceLoginStart {
            login_id: Uuid::nil(),
            authorization: OidcDeviceAuthorization {
                verification_uri: url::Url::parse("https://issuer.example/device")
                    .expect("valid fixture URL"),
                verification_uri_complete: None,
                user_code: "sentinel-user-code".to_owned(),
                expires_in: Duration::from_secs(60),
                interval: Duration::from_secs(1),
                device_code: Zeroizing::new("sentinel-device-code".to_owned()),
                verifier: Zeroizing::new("sentinel-verifier".to_owned()),
            },
            poll_secret: super::super::SecretValue::new("sentinel-poll-secret".to_owned()),
        };
        let rendered = format!("{browser:?} {device:?}");
        for secret in [
            "sentinel-state",
            "sentinel-nonce",
            "sentinel-user-code",
            "sentinel-device-code",
            "sentinel-verifier",
            "sentinel-poll-secret",
        ] {
            assert!(!rendered.contains(secret));
        }
    }

    pub(crate) async fn fixture() -> (Store, String, PgPool) {
        fixture_at_generation(1).await
    }

    pub(crate) async fn fixture_at_generation(generation: i64) -> (Store, String, PgPool) {
        let url = env::var("POHUNEK_RELAY_TEST_DATABASE_URL")
            .expect("postgres-tests requires POHUNEK_RELAY_TEST_DATABASE_URL");
        let bootstrap = Store::connect(&url, BOOTSTRAP_CONNECTIONS)
            .await
            .expect("connect PostgreSQL fixture");
        let schema = format!("relay_auth_{}", Uuid::now_v7().simple());
        sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(bootstrap.pool())
            .await
            .expect("create isolated schema");
        sqlx::raw_sql(AssertSqlSafe(format!(
            "SET search_path TO {schema};{};{}",
            include_str!("../../migrations/0001_relay_foundation.sql"),
            include_str!("../../migrations/0002_auth.sql")
        )))
        .execute(bootstrap.pool())
        .await
        .expect("run relay migrations");
        let scoped_url = format!("{url}?options[search_path]={schema}");
        let store = Store::connect(&scoped_url, TEST_CONNECTIONS)
            .await
            .expect("connect schema-scoped PostgreSQL fixture");
        sqlx::query("INSERT INTO relay_identity (relay_id,recovery_generation,state,revision) VALUES ('test-relay',$1,'normal',1)")
            .bind(generation)
            .execute(store.pool())
            .await
            .expect("seed relay identity");
        (store, schema, bootstrap.pool().clone())
    }

    pub(crate) async fn cleanup(pool: &PgPool, schema: &str) {
        sqlx::query(AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(pool)
            .await
            .expect("remove isolated schema");
    }

    async fn apply_restored_credential_change(
        store: &Store,
        credential_id: Uuid,
        assignment: &'static str,
    ) {
        let mut transaction = store
            .pool()
            .begin()
            .await
            .expect("begin restored-credential fixture");
        sqlx::query("SET LOCAL session_replication_role = replica")
            .execute(&mut *transaction)
            .await
            .expect("suppress constraints only for restored-credential fixture");
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE relay_credentials SET {assignment} WHERE credential_id = $1"
        )))
        .bind(credential_id)
        .execute(&mut *transaction)
        .await
        .expect("apply restored credential change");
        transaction
            .commit()
            .await
            .expect("commit restored-credential fixture");
    }

    pub(crate) async fn authority(store: Store) -> (Arc<Authority>, tempfile::TempDir) {
        let directory = tempfile::tempdir().expect("create witness directory");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("make witness directory private");
        let witness = Arc::new(
            WitnessStore::open(
                directory.path(),
                SigningKey::from_bytes(&[9; 32]),
                "test-key".to_owned(),
            )
            .expect("open witness"),
        );
        let recovery_generation: i64 = sqlx::query_scalar(
            "SELECT recovery_generation FROM relay_identity WHERE relay_id = 'test-relay'",
        )
        .fetch_one(store.pool())
        .await
        .expect("read fixture recovery generation");
        let current = witness
            .begin_run(None, "test-relay", recovery_generation)
            .expect("begin witness run");
        let lease = store
            .acquire_lease("test-relay", Uuid::now_v7(), recovery_generation)
            .await
            .expect("acquire lease");
        let authority = Authority::new(
            store,
            lease,
            witness,
            current,
            AuthorityLimits {
                global: 2,
                per_team: 2,
                per_principal: 2,
            },
        )
        .expect("create authority");
        (Arc::new(authority), directory)
    }

    #[tokio::test]
    async fn rotation_is_idempotent_and_revocation_invalidates_the_bearer() {
        let (store, schema, bootstrap) = fixture().await;
        let (authority, _directory) = authority(store.clone()).await;
        let service = AuthService::new(
            store.clone(),
            DigestKey::new("test".into(), b"ephemeral-test-key".to_vec()),
            2,
            limits(),
            LoginPolicy::AnyAuthenticatedSubject,
            Arc::clone(&authority),
        );
        let principal_id = Uuid::now_v7();
        let credential_id = Uuid::now_v7();
        let secret = "credential-lifecycle-old-secret";
        let identity_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO principals (id,kind,state,generation) VALUES ($1,'human','active',1)",
        )
        .bind(principal_id)
        .execute(store.pool())
        .await
        .expect("seed principal");
        sqlx::query("INSERT INTO oidc_identities (identity_id,issuer,subject,principal_id,link_generation) VALUES ($1,'issuer','rotation',$2,1)").bind(identity_id).bind(principal_id).execute(store.pool()).await.expect("seed identity");
        sqlx::query("INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,identity_id,digest_key_id,credential_kind,credential_generation,rotation_family_id,recovery_generation,expires_at) VALUES ($1,$2,$3,$4,$5,'test','human',1,$1,1,clock_timestamp()+interval '1 hour')")
            .bind(credential_id)
            .bind(credential_id.to_string())
            .bind(service.digest_key.digest(secret).as_slice())
            .bind(principal_id)
            .bind(identity_id)
            .execute(store.pool())
            .await
            .expect("seed credential");
        let actor = service
            .authenticate_bearer(RelayBearerCredential::new(format!(
                "{credential_id}.{secret}"
            )))
            .await
            .expect("authenticate seeded credential");
        let expired = RotateCredentialRequest {
            overlap_seconds: 1,
            idempotency: relay_protocol::Idempotency {
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
            expires_at: (time::OffsetDateTime::now_utc() - time::Duration::seconds(1))
                .replace_nanosecond(0)
                .expect("whole-second expired fixture"),
        };
        assert!(matches!(
            service
                .rotate_credential(actor, CredentialId::from_uuid(credential_id), expired)
                .await,
            Err(AuthError::RotationExpired)
        ));
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM relay_credentials WHERE principal_id=$1"
            )
            .bind(principal_id)
            .fetch_one(store.pool())
            .await
            .expect("credential count after expired request"),
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM mutation_receipts WHERE action='auth.credential.rotate'"
            )
            .fetch_one(store.pool())
            .await
            .expect("receipt count after expired request"),
            0
        );
        let policy_audit_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_events WHERE action = 'auth.credential.rotate'",
        )
        .fetch_one(store.pool())
        .await
        .expect("count audits before rejected policy requests");
        let policy_credential_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM relay_credentials WHERE principal_id = $1")
                .bind(principal_id)
                .fetch_one(store.pool())
                .await
                .expect("count credentials before rejected policy requests");
        let excessive_overlap = RotateCredentialRequest {
            overlap_seconds: 61,
            idempotency: relay_protocol::Idempotency {
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
            expires_at: (time::OffsetDateTime::now_utc() + time::Duration::minutes(10))
                .replace_nanosecond(0)
                .expect("whole-second excessive-overlap expiry"),
        };
        assert!(matches!(
            service
                .rotate_credential(
                    actor,
                    CredentialId::from_uuid(credential_id),
                    excessive_overlap,
                )
                .await,
            Err(AuthError::RotationRejected)
        ));
        let excessive_lifetime = RotateCredentialRequest {
            overlap_seconds: 1,
            idempotency: relay_protocol::Idempotency {
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
            expires_at: (time::OffsetDateTime::now_utc() + time::Duration::seconds(901))
                .replace_nanosecond(0)
                .expect("whole-second excessive-lifetime expiry"),
        };
        assert!(matches!(
            service
                .rotate_credential(
                    actor,
                    CredentialId::from_uuid(credential_id),
                    excessive_lifetime,
                )
                .await,
            Err(AuthError::RotationRejected)
        ));
        apply_restored_credential_change(
            &store,
            credential_id,
            "expires_at = clock_timestamp() + interval '10 seconds'",
        )
        .await;
        let insufficient_remaining_lifetime = RotateCredentialRequest {
            overlap_seconds: 30,
            idempotency: relay_protocol::Idempotency {
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
            expires_at: (time::OffsetDateTime::now_utc() + time::Duration::seconds(5))
                .replace_nanosecond(0)
                .expect("whole-second insufficient-overlap expiry"),
        };
        assert!(matches!(
            service
                .rotate_credential(
                    actor,
                    CredentialId::from_uuid(credential_id),
                    insufficient_remaining_lifetime,
                )
                .await,
            Err(AuthError::RotationRejected)
        ));
        apply_restored_credential_change(
            &store,
            credential_id,
            "expires_at = clock_timestamp() + interval '1 hour'",
        )
        .await;
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM relay_credentials WHERE principal_id = $1"
            )
            .bind(principal_id)
            .fetch_one(store.pool())
            .await
            .expect("count credentials after rejected policy requests"),
            policy_credential_count
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM audit_events WHERE action = 'auth.credential.rotate'"
            )
            .fetch_one(store.pool())
            .await
            .expect("count audits after rejected policy requests"),
            policy_audit_count
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM mutation_receipts WHERE action = 'auth.credential.rotate'"
            )
            .fetch_one(store.pool())
            .await
            .expect("count receipts after rejected policy requests"),
            0
        );
        assert!(
            !authority.is_closed(),
            "policy rejection keeps authority live"
        );
        let request = RotateCredentialRequest {
            overlap_seconds: 30,
            idempotency: relay_protocol::Idempotency {
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
            expires_at: (time::OffsetDateTime::now_utc() + time::Duration::minutes(10))
                .replace_nanosecond(0)
                .expect("whole-second fixture expiry"),
        };
        let first = service
            .rotate_credential(
                actor,
                CredentialId::from_uuid(credential_id),
                request.clone(),
            )
            .await
            .expect("rotate credential");
        let delivered = first.credential.expect("first response delivers secret");
        let new_id = first.record.credential_id;
        let replay_actor = service
            .authenticate_bearer(RelayBearerCredential::new(format!(
                "{credential_id}.{secret}"
            )))
            .await
            .expect("old overlap remains current");
        let replay = service
            .rotate_credential(
                replay_actor,
                CredentialId::from_uuid(credential_id),
                request.clone(),
            )
            .await
            .expect("exact replay succeeds");
        assert_eq!(replay.record.credential_id, new_id);
        assert!(replay.credential.is_none());
        let effects_before_repeat = (
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM relay_credentials")
                .fetch_one(store.pool())
                .await
                .expect("count credentials before repeated rotation"),
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM audit_events WHERE action = 'auth.credential.rotate'",
            )
            .fetch_one(store.pool())
            .await
            .expect("count audits before repeated rotation"),
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM mutation_receipts WHERE action = 'auth.credential.rotate'",
            )
            .fetch_one(store.pool())
            .await
            .expect("count receipts before repeated rotation"),
            authority
                .witness_sequence()
                .expect("read witness before repeated rotation"),
        );
        let rejected_repeat = service
            .rotate_credential(
                service
                    .authenticate_bearer(RelayBearerCredential::new(format!(
                        "{credential_id}.{secret}"
                    )))
                    .await
                    .expect("old overlap remains current for repeat rejection"),
                CredentialId::from_uuid(credential_id),
                RotateCredentialRequest {
                    idempotency: relay_protocol::Idempotency {
                        correlation_id: Uuid::now_v7(),
                        idempotency_key: Uuid::now_v7(),
                    },
                    ..request
                },
            )
            .await;
        assert!(matches!(rejected_repeat, Err(AuthError::RotationRejected)));
        assert_eq!(
            (
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM relay_credentials")
                    .fetch_one(store.pool())
                    .await
                    .expect("count credentials after repeated rotation"),
                sqlx::query_scalar::<_, i64>(
                    "SELECT count(*) FROM audit_events WHERE action = 'auth.credential.rotate'",
                )
                .fetch_one(store.pool())
                .await
                .expect("count audits after repeated rotation"),
                sqlx::query_scalar::<_, i64>(
                    "SELECT count(*) FROM mutation_receipts WHERE action = 'auth.credential.rotate'",
                )
                .fetch_one(store.pool())
                .await
                .expect("count receipts after repeated rotation"),
                authority.witness_sequence().expect("read witness after repeated rotation"),
            ),
            effects_before_repeat
        );
        assert!(
            !authority.is_closed(),
            "repeat rejection keeps authority live"
        );
        service
            .authenticate_bearer(RelayBearerCredential::new(format!(
                "{}.{},",
                new_id,
                delivered.secret.expose()
            )))
            .await
            .expect_err("noncanonical bearer cannot authenticate");
        let new_actor = service
            .authenticate_bearer(RelayBearerCredential::new(format!(
                "{}.{}",
                new_id,
                delivered.secret.expose()
            )))
            .await
            .expect("new credential authenticates");
        sqlx::query("UPDATE relay_credentials SET rotation_overlap_ends_at = clock_timestamp() - interval '1 second' WHERE credential_id = $1")
            .bind(credential_id)
            .execute(store.pool())
            .await
            .expect("expire predecessor overlap");
        let credential_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM relay_credentials WHERE principal_id = $1")
                .bind(principal_id)
                .fetch_one(store.pool())
                .await
                .expect("count credentials before stale rotation");
        let audit_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_events WHERE action = 'auth.credential.rotate'",
        )
        .fetch_one(store.pool())
        .await
        .expect("count audits before stale rotation");
        let stale = RotateCredentialRequest {
            overlap_seconds: 1,
            idempotency: relay_protocol::Idempotency {
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
            expires_at: first.record.expires_at,
        };
        assert!(service
            .rotate_credential(new_actor, CredentialId::from_uuid(credential_id), stale)
            .await
            .is_err());
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM relay_credentials WHERE principal_id = $1"
            )
            .bind(principal_id)
            .fetch_one(store.pool())
            .await
            .expect("count credentials after stale rotation"),
            credential_count
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM audit_events WHERE action = 'auth.credential.rotate'"
            )
            .fetch_one(store.pool())
            .await
            .expect("count audits after stale rotation"),
            audit_count
        );
        service
            .revoke_credential(new_actor, new_id)
            .await
            .expect("revoke current credential");
        service
            .authenticate_bearer(RelayBearerCredential::new(format!(
                "{}.{}",
                new_id,
                delivered.secret.expose()
            )))
            .await
            .expect_err("revoked credential cannot authenticate");
        cleanup(&bootstrap, &schema).await;
    }

    #[tokio::test]
    async fn expired_concurrent_rotation_cannot_publish_an_orphaned_credential() {
        let (store, schema, bootstrap) = fixture().await;
        let (authority, _directory) = authority(store.clone()).await;
        let service = AuthService::new(
            store.clone(),
            DigestKey::new("test".into(), b"ephemeral-test-key".to_vec()),
            2,
            limits(),
            LoginPolicy::AnyAuthenticatedSubject,
            Arc::clone(&authority),
        );
        let principal_id = Uuid::now_v7();
        let identity_id = Uuid::now_v7();
        let credential_id = Uuid::now_v7();
        let secret = "concurrent-expiry-credential";
        sqlx::query(
            "INSERT INTO principals (id,kind,state,generation) VALUES ($1,'human','active',1)",
        )
        .bind(principal_id)
        .execute(store.pool())
        .await
        .expect("seed principal");
        sqlx::query("INSERT INTO oidc_identities (identity_id,issuer,subject,principal_id,link_generation) VALUES ($1,'issuer','concurrent-expiry',$2,1)")
            .bind(identity_id)
            .bind(principal_id)
            .execute(store.pool())
            .await
            .expect("seed identity");
        sqlx::query("INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,identity_id,digest_key_id,credential_kind,credential_generation,rotation_family_id,recovery_generation,expires_at) VALUES ($1,$2,$3,$4,$5,'test','human',1,$1,1,clock_timestamp()+interval '1 hour')")
            .bind(credential_id)
            .bind(credential_id.to_string())
            .bind(service.digest_key.digest(secret).as_slice())
            .bind(principal_id)
            .bind(identity_id)
            .execute(store.pool())
            .await
            .expect("seed credential");
        let actor = service
            .authenticate_bearer(RelayBearerCredential::new(format!(
                "{credential_id}.{secret}"
            )))
            .await
            .expect("authenticate credential");
        let request = RotateCredentialRequest {
            overlap_seconds: 1,
            idempotency: relay_protocol::Idempotency {
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
            expires_at: (time::OffsetDateTime::now_utc() + time::Duration::seconds(2))
                .replace_nanosecond(0)
                .expect("whole-second concurrent expiry"),
        };
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let entered_wait = entered.notified();
        tokio::pin!(entered_wait);
        *service.rotation_hook.lock().await = Some(RotationHook {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        });
        let first_service = service.clone();
        let first_request = request.clone();
        let first = tokio::spawn(async move {
            first_service
                .rotate_credential(actor, CredentialId::from_uuid(credential_id), first_request)
                .await
        });
        entered_wait.await;
        *service.rotation_hook.lock().await = None;
        loop {
            let now: time::OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
                .fetch_one(store.pool())
                .await
                .expect("read PostgreSQL expiry clock");
            if now >= request.expires_at {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        release.notify_one();
        assert!(matches!(
            first.await.expect("join first rotation"),
            Err(AuthError::RotationExpired)
        ));
        assert!(matches!(
            service
                .rotate_credential(actor, CredentialId::from_uuid(credential_id), request)
                .await,
            Err(AuthError::RotationExpired)
        ));
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM relay_credentials WHERE principal_id = $1"
            )
            .bind(principal_id)
            .fetch_one(store.pool())
            .await
            .expect("count credentials after concurrent expiry"),
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM mutation_receipts WHERE action = 'auth.credential.rotate'"
            )
            .fetch_one(store.pool())
            .await
            .expect("count receipts after concurrent expiry"),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM audit_events WHERE action = 'auth.credential.rotate'"
            )
            .fetch_one(store.pool())
            .await
            .expect("count audit after concurrent expiry"),
            0
        );
        assert!(
            !authority.is_closed(),
            "expired retries keep authority live"
        );
        cleanup(&bootstrap, &schema).await;
    }

    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "The contention fixture must keep synchronization and durable-effect assertions together."
    )]
    async fn distinct_concurrent_rotations_retry_before_the_witness_boundary() {
        const CONTENTION_TIMEOUT: Duration = Duration::from_secs(5);

        let (store, schema, bootstrap) = fixture().await;
        let (authority, _directory) = authority(store.clone()).await;
        let limits = limits();
        assert!(
            limits.credentials_per_principal() >= 5,
            "fixture quota permits three predecessors and two rotated credentials"
        );
        let service_a = AuthService::new(
            store.clone(),
            DigestKey::new("test".into(), b"ephemeral-test-key".to_vec()),
            2,
            limits.clone(),
            LoginPolicy::AnyAuthenticatedSubject,
            Arc::clone(&authority),
        );
        let service_b = AuthService::new(
            store.clone(),
            DigestKey::new("test".into(), b"ephemeral-test-key".to_vec()),
            2,
            limits,
            LoginPolicy::AnyAuthenticatedSubject,
            Arc::clone(&authority),
        );
        let principal_id = Uuid::now_v7();
        let identity_id = Uuid::now_v7();
        let actor_credential_id = Uuid::now_v7();
        let first_predecessor_id = Uuid::now_v7();
        let second_predecessor_id = Uuid::now_v7();
        let actor_secret = "distinct-rotation-actor";
        sqlx::query(
            "INSERT INTO principals (id,kind,state,generation) VALUES ($1,'human','active',1)",
        )
        .bind(principal_id)
        .execute(store.pool())
        .await
        .expect("seed principal");
        sqlx::query("INSERT INTO oidc_identities (identity_id,issuer,subject,principal_id,link_generation) VALUES ($1,'issuer','distinct-concurrent-rotation',$2,1)")
            .bind(identity_id)
            .bind(principal_id)
            .execute(store.pool())
            .await
            .expect("seed identity");
        for (credential_id, secret) in [
            (actor_credential_id, actor_secret),
            (first_predecessor_id, "distinct-rotation-first"),
            (second_predecessor_id, "distinct-rotation-second"),
        ] {
            sqlx::query("INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,identity_id,digest_key_id,credential_kind,credential_generation,rotation_family_id,recovery_generation,expires_at) VALUES ($1,$2,$3,$4,$5,'test','human',1,$1,1,clock_timestamp()+interval '1 hour')")
                .bind(credential_id)
                .bind(credential_id.to_string())
                .bind(service_a.digest_key.digest(secret).as_slice())
                .bind(principal_id)
                .bind(identity_id)
                .execute(store.pool())
                .await
                .expect("seed active credential");
        }
        let actor = service_a
            .authenticate_bearer(RelayBearerCredential::new(format!(
                "{actor_credential_id}.{actor_secret}"
            )))
            .await
            .expect("authenticate independent actor credential");
        let expires_at = (time::OffsetDateTime::now_utc() + time::Duration::minutes(10))
            .replace_nanosecond(0)
            .expect("whole-second rotation expiry");
        let first_request = RotateCredentialRequest {
            overlap_seconds: 1,
            idempotency: relay_protocol::Idempotency {
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
            expires_at,
        };
        let second_request = RotateCredentialRequest {
            overlap_seconds: 1,
            idempotency: relay_protocol::Idempotency {
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
            expires_at,
        };
        assert_ne!(
            first_request.idempotency.idempotency_key, second_request.idempotency.idempotency_key,
            "competing rotations require distinct receipts"
        );
        let first_entered = Arc::new(tokio::sync::Notify::new());
        let first_release = Arc::new(tokio::sync::Notify::new());
        let second_entered = Arc::new(tokio::sync::Notify::new());
        let second_release = Arc::new(tokio::sync::Notify::new());
        let first_entered_wait = first_entered.notified();
        let second_entered_wait = second_entered.notified();
        tokio::pin!(first_entered_wait, second_entered_wait);
        *service_a.rotation_hook.lock().await = Some(RotationHook {
            entered: Arc::clone(&first_entered),
            release: Arc::clone(&first_release),
        });
        *service_b.rotation_hook.lock().await = Some(RotationHook {
            entered: Arc::clone(&second_entered),
            release: Arc::clone(&second_release),
        });
        let first = tokio::spawn({
            let service = service_a.clone();
            async move {
                service
                    .rotate_credential(
                        actor,
                        CredentialId::from_uuid(first_predecessor_id),
                        first_request,
                    )
                    .await
            }
        });
        let second = tokio::spawn({
            let service = service_b.clone();
            async move {
                service
                    .rotate_credential(
                        actor,
                        CredentialId::from_uuid(second_predecessor_id),
                        second_request,
                    )
                    .await
            }
        });
        tokio::time::timeout(CONTENTION_TIMEOUT, async {
            first_entered_wait.await;
            second_entered_wait.await;
        })
        .await
        .expect("both rotations reached the pre-witness barrier");
        *service_a.rotation_hook.lock().await = None;
        *service_b.rotation_hook.lock().await = None;
        first_release.notify_one();
        second_release.notify_one();
        let (first_result, second_result) =
            tokio::time::timeout(CONTENTION_TIMEOUT, async { tokio::join!(first, second) })
                .await
                .expect("competing rotations complete after the barrier opens");
        let first_result = first_result.expect("join first rotation");
        let second_result = second_result.expect("join second rotation");
        assert!(
            first_result.is_ok(),
            "first rotation result: {first_result:?}"
        );
        assert!(
            second_result.is_ok(),
            "second rotation result: {second_result:?}"
        );
        assert!(
            service_a
                .rotation_retry_count
                .load(std::sync::atomic::Ordering::Relaxed)
                + service_b
                    .rotation_retry_count
                    .load(std::sync::atomic::Ordering::Relaxed)
                >= 1,
            "one pre-witness transaction must retry after PostgreSQL contention"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM relay_credentials WHERE principal_id = $1",
            )
            .bind(principal_id)
            .fetch_one(store.pool())
            .await
            .expect("count rotated credentials"),
            5
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM relay_credentials WHERE predecessor_credential_id IN ($1, $2)",
            )
            .bind(first_predecessor_id)
            .bind(second_predecessor_id)
            .fetch_one(store.pool())
            .await
            .expect("count new credentials by predecessor"),
            2
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM audit_events WHERE action = 'auth.credential.rotate'",
            )
            .fetch_one(store.pool())
            .await
            .expect("count rotation audits"),
            2
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM mutation_receipts WHERE action = 'auth.credential.rotate'",
            )
            .fetch_one(store.pool())
            .await
            .expect("count rotation receipts"),
            2
        );
        assert!(
            !authority.is_closed(),
            "pre-witness retry keeps the authority live"
        );
        cleanup(&bootstrap, &schema).await;
    }

    #[tokio::test]
    async fn service_account_is_team_scoped_grantless_and_retry_safe() {
        let (store, schema, bootstrap) = fixture().await;
        let (authority, _directory) = authority(store.clone()).await;
        let service = AuthService::new(
            store.clone(),
            DigestKey::new("test".into(), b"ephemeral-test-key".to_vec()),
            2,
            limits(),
            LoginPolicy::AnyAuthenticatedSubject,
            authority,
        );
        let admin = Uuid::now_v7();
        let admin_credential = Uuid::now_v7();
        let admin_identity = Uuid::now_v7();
        let team = Uuid::now_v7();
        let other_team = Uuid::now_v7();
        let secret = "service-account-admin-secret";
        sqlx::query(
            "INSERT INTO principals (id,kind,state,generation) VALUES ($1,'human','active',1)",
        )
        .bind(admin)
        .execute(store.pool())
        .await
        .expect("seed admin");
        sqlx::query("INSERT INTO teams (team_id,display_name,state,policy_generation,revision) VALUES ($1,'team','active',1,1),($2,'other','active',1,1)")
            .bind(team).bind(other_team).execute(store.pool()).await.expect("seed teams");
        sqlx::query("INSERT INTO memberships (membership_id,team_id,principal_id,builtin_role,state,revision,local_deny_generation) VALUES ($1,$2,$3,'owner','active',1,1)")
            .bind(Uuid::now_v7()).bind(team).bind(admin).execute(store.pool()).await.expect("seed owner");
        sqlx::query("INSERT INTO memberships (membership_id,team_id,principal_id,builtin_role,state,revision,local_deny_generation) VALUES ($1,$2,$3,'owner','active',1,1)")
            .bind(Uuid::now_v7()).bind(other_team).bind(admin).execute(store.pool()).await.expect("seed other owner");
        sqlx::query("INSERT INTO oidc_identities (identity_id,issuer,subject,principal_id,link_generation) VALUES ($1,'issuer','service-admin',$2,1)")
            .bind(admin_identity).bind(admin).execute(store.pool()).await.expect("seed admin identity");
        sqlx::query("INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,identity_id,digest_key_id,credential_kind,credential_generation,rotation_family_id,recovery_generation,expires_at) VALUES ($1,$2,$3,$4,$5,'test','human',1,$1,1,clock_timestamp()+interval '1 hour')")
            .bind(admin_credential).bind(admin_credential.to_string()).bind(service.digest_key.digest(secret).as_slice()).bind(admin).bind(admin_identity).execute(store.pool()).await.expect("seed admin credential");
        let actor = service
            .authenticate_bearer(RelayBearerCredential::new(format!(
                "{admin_credential}.{secret}"
            )))
            .await
            .expect("authenticate owner");
        let request = CreateServiceAccountRequest {
            display_name: "deploy".to_owned(),
            idempotency: relay_protocol::Idempotency {
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
            expires_at: (time::OffsetDateTime::now_utc() + time::Duration::minutes(10))
                .replace_nanosecond(0)
                .expect("whole seconds"),
        };
        let first = service
            .create_service_account(actor, TeamId::from_uuid(team), request.clone())
            .await
            .expect("create account");
        let principal = first.account.principal_id;
        let credential = first.issued.record.credential_id;
        assert!(first.issued.credential.is_some());
        let grants: i64 = sqlx::query_scalar("SELECT count(*) FROM grants WHERE subject_id = $1")
            .bind(principal.as_uuid())
            .fetch_one(store.pool())
            .await
            .expect("count grants");
        assert_eq!(grants, 0);
        let replay = service
            .create_service_account(actor, TeamId::from_uuid(team), request.clone())
            .await
            .expect("replay account create");
        assert_eq!(replay.account.principal_id, principal);
        assert!(replay.issued.credential.is_none());
        assert!(matches!(
            service
                .create_service_account(actor, TeamId::from_uuid(other_team), request.clone())
                .await,
            Err(AuthError::IdempotencyConflict)
        ));
        let listed = service
            .list_service_accounts(
                actor,
                TeamId::from_uuid(team),
                PageRequest {
                    after: None,
                    limit: 128,
                },
            )
            .await
            .expect("list team accounts");
        assert_eq!(listed.records.len(), 1);
        assert!(service
            .list_service_accounts(
                actor,
                TeamId::from_uuid(other_team),
                PageRequest {
                    after: None,
                    limit: 128
                },
            )
            .await
            .expect("list other team")
            .records
            .is_empty());
        let rotation = RotateCredentialRequest {
            overlap_seconds: 30,
            idempotency: relay_protocol::Idempotency {
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
            expires_at: request.expires_at,
        };
        let replacement = service
            .rotate_service_credential(
                actor,
                TeamId::from_uuid(team),
                principal,
                credential,
                rotation,
            )
            .await
            .expect("rotate service credential");
        assert!(replacement.credential.is_some());
        assert!(service
            .revoke_service_credential(
                actor,
                TeamId::from_uuid(other_team),
                principal,
                replacement.record.credential_id,
            )
            .await
            .is_err());
        service
            .revoke_service_credential(
                actor,
                TeamId::from_uuid(team),
                principal,
                replacement.record.credential_id,
            )
            .await
            .expect("revoke service credential");
        cleanup(&bootstrap, &schema).await;
    }

    #[tokio::test]
    async fn revoke_retry_accepts_only_owned_or_team_scoped_inactive_credentials() {
        let (store, schema, bootstrap) = fixture().await;
        sqlx::query("UPDATE relay_identity SET recovery_generation = 2")
            .execute(store.pool())
            .await
            .expect("advance recovery generation for stale retry");
        let (authority, directory) = authority(store.clone()).await;
        let service = AuthService::new(
            store.clone(),
            DigestKey::new("test".into(), b"ephemeral-test-key".to_vec()),
            2,
            limits(),
            LoginPolicy::AnyAuthenticatedSubject,
            Arc::clone(&authority),
        );
        let principal_id = Uuid::now_v7();
        let identity_id = Uuid::now_v7();
        let current_id = Uuid::now_v7();
        let stale_id = Uuid::now_v7();
        let expired_id = Uuid::now_v7();
        let elapsed_overlap_id = Uuid::now_v7();
        let foreign_principal_id = Uuid::now_v7();
        let foreign_identity_id = Uuid::now_v7();
        let foreign_id = Uuid::now_v7();
        let service_principal_id = Uuid::now_v7();
        let service_stale_id = Uuid::now_v7();
        let service_elapsed_overlap_id = Uuid::now_v7();
        let team_id = Uuid::now_v7();
        let other_team_id = Uuid::now_v7();
        let current_secret = "current-recovery-generation-secret";
        sqlx::query("INSERT INTO principals (id,kind,state,generation) VALUES ($1,'human','active',1),($2,'human','active',1),($3,'service','active',1)")
            .bind(principal_id)
            .bind(foreign_principal_id)
            .bind(service_principal_id)
            .execute(store.pool())
            .await
            .expect("seed principals");
        sqlx::query("INSERT INTO oidc_identities (identity_id,issuer,subject,principal_id,link_generation) VALUES ($1,'issuer','owner',$2,1),($3,'issuer','foreign',$4,1)")
            .bind(identity_id)
            .bind(principal_id)
            .bind(foreign_identity_id)
            .bind(foreign_principal_id)
            .execute(store.pool())
            .await
            .expect("seed identities");
        sqlx::query("INSERT INTO teams (team_id,display_name,state,policy_generation,revision) VALUES ($1,'retry-team','active',1,1),($2,'other-team','active',1,1)")
            .bind(team_id)
            .bind(other_team_id)
            .execute(store.pool())
            .await
            .expect("seed teams");
        sqlx::query("INSERT INTO memberships (membership_id,team_id,principal_id,builtin_role,state,revision,local_deny_generation) VALUES ($1,$2,$3,'owner','active',1,1)")
            .bind(Uuid::now_v7())
            .bind(team_id)
            .bind(principal_id)
            .execute(store.pool())
            .await
            .expect("seed owner membership");
        sqlx::query("INSERT INTO service_accounts (principal_id,team_id,display_name) VALUES ($1,$2,'retry-service')")
            .bind(service_principal_id)
            .bind(team_id)
            .execute(store.pool())
            .await
            .expect("seed service account");
        sqlx::query("INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,identity_id,digest_key_id,credential_kind,credential_generation,rotation_family_id,recovery_generation,issued_at,expires_at) VALUES ($1,$2,$3,$4,$5,'test','human',1,$1,2,clock_timestamp()-interval '2 hours',clock_timestamp()+interval '1 hour'),($6,$7,decode(repeat('01',32),'hex'),$4,$5,'test','human',1,$6,1,clock_timestamp()-interval '2 hours',clock_timestamp()+interval '1 hour'),($8,$9,decode(repeat('02',32),'hex'),$4,$5,'test','human',1,$8,2,clock_timestamp()-interval '2 hours',clock_timestamp()-interval '1 second'),($10,$11,decode(repeat('03',32),'hex'),$12,$13,'test','human',1,$10,1,clock_timestamp()-interval '2 hours',clock_timestamp()+interval '1 hour'),($14,$15,decode(repeat('04',32),'hex'),$16,NULL,'test','service',1,$14,1,clock_timestamp()-interval '2 hours',clock_timestamp()+interval '1 hour')")
            .bind(current_id).bind(current_id.to_string()).bind(service.digest_key.digest(current_secret).as_slice()).bind(principal_id).bind(identity_id)
            .bind(stale_id).bind(stale_id.to_string())
            .bind(expired_id).bind(expired_id.to_string())
            .bind(foreign_id).bind(foreign_id.to_string()).bind(foreign_principal_id).bind(foreign_identity_id)
            .bind(service_stale_id).bind(service_stale_id.to_string()).bind(service_principal_id)
            .execute(store.pool()).await.expect("seed current and inactive credentials");
        sqlx::query("INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,identity_id,digest_key_id,credential_kind,credential_generation,rotation_family_id,recovery_generation,rotation_overlap_ends_at,expires_at) VALUES ($1,$2,decode(repeat('05',32),'hex'),$3,$4,'test','human',1,$1,2,clock_timestamp()-interval '1 second',clock_timestamp()+interval '1 hour'),($5,$6,decode(repeat('06',32),'hex'),$7,NULL,'test','service',1,$5,2,clock_timestamp()-interval '1 second',clock_timestamp()+interval '1 hour')")
            .bind(elapsed_overlap_id)
            .bind(elapsed_overlap_id.to_string())
            .bind(principal_id)
            .bind(identity_id)
            .bind(service_elapsed_overlap_id)
            .bind(service_elapsed_overlap_id.to_string())
            .bind(service_principal_id)
            .execute(store.pool())
            .await
            .expect("seed elapsed-overlap credentials");
        let actor = service
            .authenticate_bearer(RelayBearerCredential::new(format!(
                "{current_id}.{current_secret}"
            )))
            .await
            .expect("authenticate current generation credential");
        let guard = authority
            .admit(AuthorizationRequest {
                actor: actor.actor(),
                team_id,
                permission: "team.membership.read".to_owned(),
                resource: ResourceScope::Team,
                correlation_id: Uuid::now_v7(),
            })
            .await
            .expect("admit unrelated owner access");
        let audit_before: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
            .fetch_one(store.pool())
            .await
            .expect("count audit before inactive retries");
        let mut witness_before = std::fs::read_dir(directory.path())
            .expect("list witness before inactive retries")
            .map(|entry| entry.expect("read witness entry").file_name())
            .collect::<Vec<_>>();
        witness_before.sort();
        service
            .revoke_credential(actor, CredentialId::from_uuid(stale_id))
            .await
            .expect("stale owned credential is already desired state");
        service
            .revoke_credential(actor, CredentialId::from_uuid(expired_id))
            .await
            .expect("expired owned credential is already desired state");
        service
            .revoke_credential(actor, CredentialId::from_uuid(elapsed_overlap_id))
            .await
            .expect("elapsed-overlap owned credential is already desired state");
        service
            .revoke_service_credential(
                actor,
                TeamId::from_uuid(team_id),
                PrincipalId::from_uuid(service_principal_id),
                CredentialId::from_uuid(service_stale_id),
            )
            .await
            .expect("stale team service credential is already desired state");
        service
            .revoke_service_credential(
                actor,
                TeamId::from_uuid(team_id),
                PrincipalId::from_uuid(service_principal_id),
                CredentialId::from_uuid(service_elapsed_overlap_id),
            )
            .await
            .expect("elapsed-overlap service credential is already desired state");
        assert!(matches!(
            service
                .revoke_credential(actor, CredentialId::from_uuid(foreign_id))
                .await,
            Err(AuthError::CredentialInvalid)
        ));
        assert!(matches!(
            service
                .revoke_service_credential(
                    actor,
                    TeamId::from_uuid(other_team_id),
                    PrincipalId::from_uuid(service_principal_id),
                    CredentialId::from_uuid(service_stale_id),
                )
                .await,
            Err(AuthError::CredentialInvalid)
        ));
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM audit_events")
                .fetch_one(store.pool())
                .await
                .expect("count audit after inactive retries"),
            audit_before
        );
        for credential_id in [elapsed_overlap_id, service_elapsed_overlap_id] {
            let row = sqlx::query(
                "SELECT credential_generation, revoked_at FROM relay_credentials WHERE credential_id = $1",
            )
            .bind(credential_id)
            .fetch_one(store.pool())
            .await
            .expect("read elapsed-overlap credential after retry");
            assert_eq!(row.get::<i64, _>("credential_generation"), 1);
            assert!(row
                .get::<Option<time::OffsetDateTime>, _>("revoked_at")
                .is_none());
        }
        let mut witness_after = std::fs::read_dir(directory.path())
            .expect("list witness after inactive retries")
            .map(|entry| entry.expect("read witness entry").file_name())
            .collect::<Vec<_>>();
        witness_after.sort();
        assert_eq!(
            witness_after, witness_before,
            "inactive retries do not append a witness"
        );
        assert!(
            !authority.is_closed(),
            "inactive retries keep authority live"
        );
        assert!(
            !guard.cancelled(),
            "inactive retries do not cancel unrelated access"
        );
        guard
            .validate()
            .await
            .expect("unrelated access remains valid after retries");
        cleanup(&bootstrap, &schema).await;
    }

    #[tokio::test]
    async fn revoke_retry_rechecks_an_actor_that_expires_while_the_target_is_locked() {
        let (store, schema, bootstrap) = fixture().await;
        let (authority, directory) = authority(store.clone()).await;
        let service = AuthService::new(
            store.clone(),
            DigestKey::new("test".into(), b"ephemeral-test-key".to_vec()),
            2,
            limits(),
            LoginPolicy::AnyAuthenticatedSubject,
            Arc::clone(&authority),
        );
        let principal_id = Uuid::now_v7();
        let identity_id = Uuid::now_v7();
        let actor_credential_id = Uuid::now_v7();
        let target_credential_id = Uuid::now_v7();
        let secret = "revoke-final-currentness-secret";
        sqlx::query(
            "INSERT INTO principals (id,kind,state,generation) VALUES ($1,'human','active',1)",
        )
        .bind(principal_id)
        .execute(store.pool())
        .await
        .expect("seed principal");
        sqlx::query("INSERT INTO oidc_identities (identity_id,issuer,subject,principal_id,link_generation) VALUES ($1,'issuer','revoke-currentness',$2,1)")
            .bind(identity_id)
            .bind(principal_id)
            .execute(store.pool())
            .await
            .expect("seed identity");
        sqlx::query("INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,identity_id,digest_key_id,credential_kind,credential_generation,rotation_family_id,recovery_generation,rotation_overlap_ends_at,expires_at) VALUES ($1,$2,$3,$4,$5,'test','human',1,$1,1,NULL,clock_timestamp()+interval '2 seconds'),($6,$7,decode(repeat('07',32),'hex'),$4,$5,'test','human',1,$6,1,NULL,clock_timestamp()+interval '2 seconds')")
            .bind(actor_credential_id)
            .bind(actor_credential_id.to_string())
            .bind(service.digest_key.digest(secret).as_slice())
            .bind(principal_id)
            .bind(identity_id)
            .bind(target_credential_id)
            .bind(target_credential_id.to_string())
            .execute(store.pool())
            .await
            .expect("seed actor and locked target credentials");
        let actor = service
            .authenticate_bearer(RelayBearerCredential::new(format!(
                "{actor_credential_id}.{secret}"
            )))
            .await
            .expect("authenticate actor before expiry");
        let mut witness_before = std::fs::read_dir(directory.path())
            .expect("list witness before stale actor retry")
            .map(|entry| entry.expect("read witness entry").file_name())
            .collect::<Vec<_>>();
        witness_before.sort();
        let mut target_lock = store
            .begin_serializable()
            .await
            .expect("begin target lock transaction");
        sqlx::query(
            "SELECT credential_id FROM relay_credentials WHERE credential_id = $1 FOR UPDATE",
        )
        .bind(target_credential_id)
        .fetch_one(&mut *target_lock)
        .await
        .expect("hold target credential lock");
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let entered_wait = entered.notified();
        tokio::pin!(entered_wait);
        *service.revoke_hook.lock().await = Some(RotationHook {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        });
        let retry_service = service.clone();
        let retry = tokio::spawn(async move {
            retry_service
                .revoke_credential(actor, CredentialId::from_uuid(target_credential_id))
                .await
        });
        entered_wait.await;
        *service.revoke_hook.lock().await = None;
        release.notify_one();
        loop {
            let now: time::OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
                .fetch_one(store.pool())
                .await
                .expect("read PostgreSQL expiry clock");
            let expires_at: time::OffsetDateTime = sqlx::query_scalar(
                "SELECT expires_at FROM relay_credentials WHERE credential_id = $1",
            )
            .bind(actor_credential_id)
            .fetch_one(store.pool())
            .await
            .expect("read actor expiry");
            if now >= expires_at {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        target_lock
            .commit()
            .await
            .expect("release expired target lock");
        let retry_result = retry.await.expect("join revoke retry");
        assert!(
            matches!(retry_result, Err(AuthError::CredentialInvalid)),
            "stale actor retry result: {retry_result:?}"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM audit_events")
                .fetch_one(store.pool())
                .await
                .expect("count audit after stale actor retry"),
            0
        );
        let target = sqlx::query(
            "SELECT credential_generation, revoked_at FROM relay_credentials WHERE credential_id = $1",
        )
        .bind(target_credential_id)
        .fetch_one(store.pool())
        .await
        .expect("read active target after stale actor retry");
        assert_eq!(target.get::<i64, _>("credential_generation"), 1);
        assert!(target
            .get::<Option<time::OffsetDateTime>, _>("revoked_at")
            .is_none());
        let mut witness_after = std::fs::read_dir(directory.path())
            .expect("list witness after stale actor retry")
            .map(|entry| entry.expect("read witness entry").file_name())
            .collect::<Vec<_>>();
        witness_after.sort();
        assert_eq!(
            witness_after, witness_before,
            "stale actor retry does not append a witness"
        );
        assert!(
            !authority.is_closed(),
            "stale actor retry does not fail-stop authority"
        );
        cleanup(&bootstrap, &schema).await;
    }

    #[tokio::test]
    async fn concurrent_device_issuance_at_credential_cap_commits_once() {
        let (store, schema, bootstrap) = fixture().await;
        let (authority, _directory) = authority(store.clone()).await;
        let limits = AuthLimits::new(
            Duration::from_secs(300),
            Duration::from_secs(600),
            Duration::from_secs(60),
            Duration::from_secs(900),
            Duration::from_secs(900),
            Duration::from_secs(60),
            1,
            16,
            Duration::from_secs(30),
            Duration::from_secs(5),
            Duration::from_secs(60),
        )
        .expect("cap-one limits");
        let service = AuthService::new(
            store.clone(),
            DigestKey::new("test".into(), b"ephemeral-test-key".to_vec()),
            2,
            limits,
            LoginPolicy::AnyAuthenticatedSubject,
            authority,
        );
        let principal_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO principals (id,kind,state,generation) VALUES ($1,'human','active',1)",
        )
        .bind(principal_id)
        .execute(store.pool())
        .await
        .expect("seed principal");
        sqlx::query("INSERT INTO oidc_identities (identity_id,issuer,subject,principal_id,link_generation) VALUES ($1,'issuer','subject',$2,1)").bind(Uuid::now_v7()).bind(principal_id).execute(store.pool()).await.expect("seed identity");
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();
        let first_token = [1; RANDOM_SECRET_BYTES];
        let second_token = [2; RANDOM_SECRET_BYTES];
        insert_current_device_login(&store, first, &first_token).await;
        sqlx::query("UPDATE device_logins SET device_code_digest = decode(repeat('04',32),'hex') WHERE login_id=$1")
            .bind(first)
            .execute(store.pool())
            .await
            .expect("separate first device code");
        insert_current_device_login(&store, second, &second_token).await;
        let one = DeviceIssueBinding {
            issuer: "issuer".into(),
            client_id: "client".into(),
            login_id: first,
            recovery_generation: 1,
            poll_interval_seconds: 7,
            poll_lease_token: first_token,
        };
        let two = DeviceIssueBinding {
            issuer: "issuer".into(),
            client_id: "client".into(),
            login_id: second,
            recovery_generation: 1,
            poll_interval_seconds: 7,
            poll_lease_token: second_token,
        };
        let (left, right) = tokio::join!(
            service.issue_human_credential("subject".into(), one),
            service.issue_human_credential("subject".into(), two)
        );
        assert_eq!(
            usize::from(left.is_ok()) + usize::from(right.is_ok()),
            1,
            "left={left:?} right={right:?}"
        );
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM relay_credentials WHERE principal_id=$1 AND revoked_at IS NULL",
        )
        .bind(principal_id)
        .fetch_one(store.pool())
        .await
        .expect("credential count");
        assert_eq!(count, 1);
        let audits: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_events WHERE action='auth.human_credential.issue'",
        )
        .fetch_one(store.pool())
        .await
        .expect("audit count");
        assert_eq!(audits, 1);
        cleanup(&bootstrap, &schema).await;
    }

    async fn insert_current_device_login(
        store: &Store,
        login_id: Uuid,
        poll_lease_token: &[u8; RANDOM_SECRET_BYTES],
    ) {
        sqlx::query(
            "INSERT INTO device_logins (login_id,device_code_digest,poll_secret_digest,issuer,client_id,audience,action,account_link_generation,recovery_generation,expires_at,poll_interval_seconds,next_poll_at,poll_lease_until,poll_lease_token) VALUES ($1,decode(repeat('01',32),'hex'),decode(repeat('02',32),'hex'),'issuer','client','client','login',1,1,clock_timestamp()+interval '1 hour',7,clock_timestamp(),clock_timestamp()+interval '1 hour',$2)",
        )
        .bind(login_id)
        .bind(poll_lease_token.as_slice())
        .execute(store.pool())
        .await
            .expect("seed current device login");
    }

    async fn insert_consumed_browser_login(store: &Store, login_id: Uuid) {
        sqlx::query(
            "INSERT INTO browser_logins (login_id,state_digest,nonce_digest,pkce_verifier_digest,login_binding_digest,issuer,client_id,audience,redirect_uri,action,account_link_generation,recovery_generation,expires_at,consumed_at,outcome) VALUES ($1,decode(repeat('03',32),'hex'),decode(repeat('04',32),'hex'),decode(repeat('05',32),'hex'),decode(repeat('06',32),'hex'),'issuer','client','client','https://relay.example/callback','login',1,1,clock_timestamp()+interval '1 hour',clock_timestamp(),'failed')",
        )
        .bind(login_id)
        .execute(store.pool())
        .await
        .expect("seed consumed browser login");
    }

    async fn insert_callback_login(
        store: &Store,
        key: &DigestKey,
        login_id: Uuid,
        state: &str,
        binding: &str,
        nonce: &str,
        verifier: &str,
    ) {
        sqlx::query(
            "INSERT INTO browser_logins (login_id,state_digest,nonce_digest,pkce_verifier_digest,login_binding_digest,issuer,client_id,audience,redirect_uri,action,account_link_generation,recovery_generation,expires_at) VALUES ($1,$2,$3,$4,$5,'https://issuer.example','client','client','https://relay.example/callback','login',1,1,clock_timestamp()+interval '1 hour')",
        )
        .bind(login_id)
        .bind(key.digest(state).as_slice())
        .bind(key.digest(nonce).as_slice())
        .bind(key.digest(verifier).as_slice())
        .bind(key.digest(binding).as_slice())
        .execute(store.pool())
        .await
        .expect("seed bound browser callback");
    }

    #[tokio::test]
    async fn terminal_bound_callbacks_consume_pending_verifiers_and_audit_denials() {
        let (store, schema, bootstrap) = fixture().await;
        let (authority, _directory) = authority(store.clone()).await;
        let key = DigestKey::new("test".into(), b"ephemeral-test-key".to_vec());
        let service = AuthService::new(
            store.clone(),
            key.clone(),
            4,
            limits(),
            LoginPolicy::AnyAuthenticatedSubject,
            authority,
        );
        let oidc = OidcClient::test_client();
        for (outcome, issuer, session_state, expected) in [
            (
                BrowserCallbackOutcome::ProviderDenied,
                None,
                None,
                "callback_invalid",
            ),
            (
                BrowserCallbackOutcome::Invalid,
                Some("https://wrong.example".to_owned()),
                None,
                "callback_invalid",
            ),
            (
                BrowserCallbackOutcome::Invalid,
                None,
                Some(String::new()),
                "callback_invalid",
            ),
        ] {
            let login_id = Uuid::now_v7();
            insert_callback_login(
                &store, &key, login_id, "state", "binding", "nonce", "verifier",
            )
            .await;
            service.pending_browser_logins.lock().await.insert(
                login_id,
                PendingBrowserLogin {
                    verifier: Zeroizing::new("verifier".to_owned()),
                    nonce: Zeroizing::new("nonce".to_owned()),
                    expires_at: Instant::now() + Duration::from_secs(60),
                },
            );
            assert!(matches!(
                service
                    .complete_browser_login(
                        &oidc,
                        BrowserCallback::new("state".to_owned(), issuer, session_state, outcome),
                        LoginBindingCookie::new("binding".to_owned()),
                    )
                    .await,
                Err(AuthError::OidcInvalid)
            ));
            assert!(!service
                .pending_browser_logins
                .lock()
                .await
                .contains_key(&login_id));
            let audited: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='auth.browser.callback' AND decision='deny' AND outcome=$1")
                .bind(expected).fetch_one(store.pool()).await.expect("read denial audit");
            assert!(audited >= 1);
        }
        let missing_id = Uuid::now_v7();
        insert_callback_login(
            &store, &key, missing_id, "missing", "binding", "nonce", "verifier",
        )
        .await;
        assert!(matches!(
            service
                .complete_browser_login(
                    &oidc,
                    BrowserCallback::new(
                        "missing".to_owned(),
                        None,
                        None,
                        BrowserCallbackOutcome::Code(super::super::OidcCallbackCode::new(
                            "code".to_owned()
                        ))
                    ),
                    LoginBindingCookie::new("binding".to_owned()),
                )
                .await,
            Err(AuthError::OidcInvalid)
        ));
        let missing_audit: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='auth.browser.callback' AND outcome='verifier_missing'")
            .fetch_one(store.pool()).await.expect("read missing verifier audit");
        assert_eq!(missing_audit, 1);
        assert!(matches!(
            service
                .complete_browser_login(
                    &oidc,
                    BrowserCallback::new(
                        "missing".to_owned(),
                        None,
                        None,
                        BrowserCallbackOutcome::Code(super::super::OidcCallbackCode::new(
                            "replacement-code".to_owned()
                        ))
                    ),
                    LoginBindingCookie::new("binding".to_owned()),
                )
                .await,
            Err(AuthError::OidcInvalid)
        ));
        let replay_audit: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='auth.browser.callback' AND outcome='rejected'")
            .fetch_one(store.pool()).await.expect("read replay audit");
        assert_eq!(replay_audit, 1);
        cleanup(&bootstrap, &schema).await;
    }

    #[test]
    fn limits_reject_incoherent_idle_or_poll_windows() {
        assert!(AuthLimits::new(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            1,
            1,
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .is_err());
    }

    #[tokio::test]
    async fn audit_failure_rolls_back_browser_session_issue() {
        let (store, schema, bootstrap) = fixture().await;
        let (authority, _directory) = authority(store.clone()).await;
        let service = AuthService::new(
            store.clone(),
            DigestKey::new("test".into(), b"ephemeral-test-key".to_vec()),
            2,
            limits(),
            LoginPolicy::AnyAuthenticatedSubject,
            authority,
        );
        sqlx::raw_sql(
            "CREATE FUNCTION reject_auth_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'audit unavailable'; END $$; \
             CREATE TRIGGER reject_auth_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION reject_auth_audit();",
        )
        .execute(store.pool())
        .await
        .expect("install audit failure trigger");
        let login_id = Uuid::now_v7();
        insert_consumed_browser_login(&store, login_id).await;
        let error = service
            .issue_browser_session(
                "issuer".into(),
                "subject".into(),
                "client".into(),
                "https://relay.example/callback".into(),
                login_id,
                1,
            )
            .await
            .expect_err("audit failure denies session issue");
        assert!(matches!(error, AuthError::Durable));
        let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM browser_sessions")
            .fetch_one(store.pool())
            .await
            .expect("count sessions");
        let principals: i64 = sqlx::query_scalar("SELECT count(*) FROM principals")
            .fetch_one(store.pool())
            .await
            .expect("count principals");
        assert_eq!(sessions, 0);
        assert_eq!(principals, 0);
        cleanup(&bootstrap, &schema).await;
    }

    #[tokio::test]
    async fn browser_touch_caps_idle_deadline_at_absolute_expiry() {
        let (store, schema, bootstrap) = fixture().await;
        let principal = Uuid::now_v7();
        let identity = Uuid::now_v7();
        let session = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO principals (id,kind,state,generation) VALUES ($1,'human','active',1)",
        )
        .bind(principal)
        .execute(store.pool())
        .await
        .expect("seed principal");
        sqlx::query("INSERT INTO oidc_identities (identity_id,issuer,subject,principal_id,link_generation) VALUES ($1,'issuer','browser-touch',$2,1)").bind(identity).bind(principal).execute(store.pool()).await.expect("seed identity");
        sqlx::query("INSERT INTO browser_sessions (session_id,cookie_digest,csrf_digest,principal_id,identity_id,digest_key_id,session_generation,recovery_generation,expires_at,idle_deadline) VALUES ($1,decode(repeat('01',32),'hex'),decode(repeat('02',32),'hex'),$2,$3,'test',1,1,clock_timestamp()+interval '2 seconds',clock_timestamp()+interval '1 second')")
            .bind(session)
            .bind(principal)
            .bind(identity)
            .execute(store.pool())
            .await
            .expect("seed session");
        let mut transaction = store
            .begin_serializable()
            .await
            .expect("begin touch transaction");
        touch_browser_session(&mut transaction, session, Duration::from_secs(60))
            .await
            .expect("touch current session");
        transaction
            .commit()
            .await
            .expect("commit touch transaction");
        let capped: bool = sqlx::query_scalar(
            "SELECT idle_deadline = expires_at FROM browser_sessions WHERE session_id=$1",
        )
        .bind(session)
        .fetch_one(store.pool())
        .await
        .expect("read deadline");
        assert!(capped);
        cleanup(&bootstrap, &schema).await;
    }

    #[tokio::test]
    async fn concurrent_first_login_uses_one_canonical_principal() {
        let (store, schema, bootstrap) = fixture().await;
        let key = DigestKey::new("test".into(), b"ephemeral-test-key".to_vec());
        let (authority, _directory) = authority(store.clone()).await;
        let first = AuthService::new(
            store.clone(),
            key.clone(),
            2,
            limits(),
            LoginPolicy::AnyAuthenticatedSubject,
            authority.clone(),
        );
        let second = AuthService::new(
            store.clone(),
            key,
            2,
            limits(),
            LoginPolicy::AnyAuthenticatedSubject,
            authority,
        );
        let first_login = Uuid::now_v7();
        let second_login = Uuid::now_v7();
        insert_consumed_browser_login(&store, first_login).await;
        insert_consumed_browser_login(&store, second_login).await;
        let (first, second) = tokio::join!(
            first.issue_browser_session(
                "issuer".into(),
                "subject".into(),
                "client".into(),
                "https://relay.example/callback".into(),
                first_login,
                1
            ),
            second.issue_browser_session(
                "issuer".into(),
                "subject".into(),
                "client".into(),
                "https://relay.example/callback".into(),
                second_login,
                1
            ),
        );
        first.expect("first login succeeds");
        second.expect("concurrent login succeeds");
        let principals: i64 = sqlx::query_scalar("SELECT count(*) FROM principals")
            .fetch_one(store.pool())
            .await
            .expect("count principals");
        let identities: i64 = sqlx::query_scalar("SELECT count(*) FROM oidc_identities")
            .fetch_one(store.pool())
            .await
            .expect("count identities");
        let session_principals: i64 =
            sqlx::query_scalar("SELECT count(DISTINCT principal_id) FROM browser_sessions")
                .fetch_one(store.pool())
                .await
                .expect("count canonical session principals");
        assert_eq!(principals, 1);
        assert_eq!(identities, 1);
        assert_eq!(session_principals, 1);
        cleanup(&bootstrap, &schema).await;
    }

    #[tokio::test]
    async fn allowed_subjects_deny_before_principal_or_credential_mutation() {
        let (store, schema, bootstrap) = fixture().await;
        let (authority, _directory) = authority(store.clone()).await;
        let service = AuthService::new(
            store.clone(),
            DigestKey::new("test".into(), b"ephemeral-test-key".to_vec()),
            2,
            limits(),
            LoginPolicy::AllowedSubjects(vec!["allowed-subject".to_owned()]),
            authority,
        );
        let login_id = Uuid::now_v7();
        insert_consumed_browser_login(&store, login_id).await;
        let error = service
            .issue_browser_session(
                "issuer".into(),
                "blocked-subject".into(),
                "client".into(),
                "https://relay.example/callback".into(),
                login_id,
                1,
            )
            .await
            .expect_err("configured policy rejects an unmatched OIDC subject");
        assert!(matches!(error, AuthError::CredentialInvalid));
        let principals: i64 = sqlx::query_scalar("SELECT count(*) FROM principals")
            .fetch_one(store.pool())
            .await
            .expect("count principals");
        let credentials: i64 = sqlx::query_scalar("SELECT count(*) FROM relay_credentials")
            .fetch_one(store.pool())
            .await
            .expect("count credentials");
        let denies: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_events WHERE action='auth.browser.session.issue' AND decision='deny' AND outcome='policy_denied'",
        )
        .fetch_one(store.pool())
        .await
        .expect("count policy denials");
        assert_eq!(principals, 0);
        assert_eq!(credentials, 0);
        assert_eq!(denies, 1);
        cleanup(&bootstrap, &schema).await;
    }

    #[tokio::test]
    async fn replaced_poll_lease_cannot_issue_a_credential() {
        let (store, schema, bootstrap) = fixture().await;
        let (authority, _directory) = authority(store.clone()).await;
        let service = AuthService::new(
            store.clone(),
            DigestKey::new("test".into(), b"ephemeral-test-key".to_vec()),
            2,
            limits(),
            LoginPolicy::AnyAuthenticatedSubject,
            authority,
        );
        let login_id = Uuid::now_v7();
        let old_token = [3; RANDOM_SECRET_BYTES];
        let successor_token = [4; RANDOM_SECRET_BYTES];
        insert_current_device_login(&store, login_id, &successor_token).await;
        let error = service
            .issue_human_credential_once(
                "subject".to_owned(),
                &DeviceIssueBinding {
                    issuer: "issuer".to_owned(),
                    client_id: "client".to_owned(),
                    login_id,
                    recovery_generation: 1,
                    poll_interval_seconds: 7,
                    poll_lease_token: old_token,
                },
            )
            .await
            .expect_err("a response owned by an expired lease cannot issue");
        assert!(matches!(error, AuthError::DeviceBusy { .. }));
        let credentials: i64 = sqlx::query_scalar("SELECT count(*) FROM relay_credentials")
            .fetch_one(store.pool())
            .await
            .expect("count credentials");
        let consumed: bool = sqlx::query_scalar(
            "SELECT consumed_at IS NOT NULL FROM device_logins WHERE login_id=$1",
        )
        .bind(login_id)
        .fetch_one(store.pool())
        .await
        .expect("read device login");
        assert_eq!(credentials, 0);
        assert!(!consumed);
        cleanup(&bootstrap, &schema).await;
    }

    #[tokio::test]
    async fn cancelled_or_expired_device_login_cannot_issue_a_credential() {
        let (store, schema, bootstrap) = fixture().await;
        let (authority, _directory) = authority(store.clone()).await;
        let service = AuthService::new(
            store.clone(),
            DigestKey::new("test".into(), b"ephemeral-test-key".to_vec()),
            2,
            limits(),
            LoginPolicy::AnyAuthenticatedSubject,
            authority,
        );
        for (token_byte, terminal_sql) in [
            (
                6_u8,
                "UPDATE device_logins SET consumed_at=clock_timestamp(), outcome='cancelled' WHERE login_id=$1",
            ),
            (
                7_u8,
                "UPDATE device_logins SET created_at=clock_timestamp()-interval '2 hours', expires_at=clock_timestamp()-interval '1 hour' WHERE login_id=$1",
            ),
        ] {
            let login_id = Uuid::now_v7();
            let token = [token_byte; RANDOM_SECRET_BYTES];
            insert_current_device_login(&store, login_id, &token).await;
            sqlx::query(terminal_sql)
                .bind(login_id)
                .execute(store.pool())
                .await
                .expect("make device transaction terminal");
            let error = service
                .issue_human_credential_once(
                    "subject".to_owned(),
                    &DeviceIssueBinding {
                        issuer: "issuer".to_owned(),
                        client_id: "client".to_owned(),
                        login_id,
                        recovery_generation: 1,
                        poll_interval_seconds: 7,
                        poll_lease_token: token,
                    },
                )
                .await
                .expect_err("terminal device login cannot issue");
            assert!(matches!(error, AuthError::DeviceBusy { .. }));
        }
        let credentials: i64 = sqlx::query_scalar("SELECT count(*) FROM relay_credentials")
            .fetch_one(store.pool())
            .await
            .expect("count credentials");
        assert_eq!(credentials, 0);
        cleanup(&bootstrap, &schema).await;
    }

    #[tokio::test]
    async fn audit_failure_rolls_back_human_credential_issue() {
        let (store, schema, bootstrap) = fixture().await;
        let (authority, _directory) = authority(store.clone()).await;
        let service = AuthService::new(
            store.clone(),
            DigestKey::new("test".into(), b"ephemeral-test-key".to_vec()),
            2,
            limits(),
            LoginPolicy::AnyAuthenticatedSubject,
            authority,
        );
        let login_id = Uuid::now_v7();
        let token = [5; RANDOM_SECRET_BYTES];
        insert_current_device_login(&store, login_id, &token).await;
        sqlx::raw_sql(
            "CREATE FUNCTION reject_auth_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'audit unavailable'; END $$; \
             CREATE TRIGGER reject_auth_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION reject_auth_audit();",
        )
        .execute(store.pool())
        .await
        .expect("install audit failure trigger");
        let error = service
            .issue_human_credential_once(
                "subject".to_owned(),
                &DeviceIssueBinding {
                    issuer: "issuer".to_owned(),
                    client_id: "client".to_owned(),
                    login_id,
                    recovery_generation: 1,
                    poll_interval_seconds: 7,
                    poll_lease_token: token,
                },
            )
            .await
            .expect_err("audit failure denies credential issue");
        assert!(matches!(error, AuthError::Durable));
        let credentials: i64 = sqlx::query_scalar("SELECT count(*) FROM relay_credentials")
            .fetch_one(store.pool())
            .await
            .expect("count credentials");
        let principals: i64 = sqlx::query_scalar("SELECT count(*) FROM principals")
            .fetch_one(store.pool())
            .await
            .expect("count principals");
        assert_eq!(credentials, 0);
        assert_eq!(principals, 0);
        cleanup(&bootstrap, &schema).await;
    }

    #[tokio::test]
    async fn infrastructure_human_credential_preserves_kind_without_team_access() {
        let (store, schema, bootstrap) = fixture().await;
        let (authority, _directory) = authority(store.clone()).await;
        let service = AuthService::new(
            store.clone(),
            DigestKey::new("test".into(), b"ephemeral-test-key".to_vec()),
            2,
            limits(),
            LoginPolicy::AnyAuthenticatedSubject,
            authority.clone(),
        );
        let principal_id = Uuid::now_v7();
        let credential_id = Uuid::now_v7();
        let secret = "infrastructure-human-credential";
        let identity_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO principals (id,kind,state,generation) VALUES ($1,'infrastructure','active',1)",
        )
        .bind(principal_id)
        .execute(store.pool())
        .await
        .expect("seed infrastructure principal");
        sqlx::query("INSERT INTO oidc_identities (identity_id,issuer,subject,principal_id,link_generation) VALUES ($1,'issuer','infra',$2,1)").bind(identity_id).bind(principal_id).execute(store.pool()).await.expect("seed identity");
        sqlx::query("INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,identity_id,digest_key_id,credential_kind,credential_generation,rotation_family_id,recovery_generation,expires_at) VALUES ($1,$2,$3,$4,$5,'test','human',1,$1,1,clock_timestamp()+interval '1 hour')")
            .bind(credential_id)
            .bind(credential_id.to_string())
            .bind(service.digest_key.digest(secret).as_slice())
            .bind(principal_id)
            .bind(identity_id)
            .execute(store.pool())
            .await
            .expect("seed infrastructure human credential");
        let actor = service
            .authenticate_bearer(RelayBearerCredential::new(format!(
                "{credential_id}.{secret}"
            )))
            .await
            .expect("authenticate infrastructure human credential")
            .actor();
        assert_eq!(actor.kind(), ActorKind::Infrastructure);

        let team_id = Uuid::now_v7();
        sqlx::query("INSERT INTO teams (team_id,display_name,state,policy_generation,revision) VALUES ($1,'unassigned','active',1,1)")
            .bind(team_id)
            .execute(store.pool())
            .await
            .expect("seed unrelated team");
        let denial = authority
            .admit(crate::store::AuthorizationRequest {
                actor,
                team_id,
                permission: "session.metadata.read".to_owned(),
                resource: crate::store::ResourceScope::Team,
                correlation_id: Uuid::now_v7(),
            })
            .await
            .expect_err("authentication cannot create an implicit team grant");
        assert!(matches!(denial, crate::admission::AuthorityError::Store(_)));
        cleanup(&bootstrap, &schema).await;
    }

    #[test]
    fn credential_actor_kind_uses_durable_principal_kind() {
        assert!(matches!(
            credential_actor_kind("infrastructure".to_owned(), "human".to_owned()),
            Ok(ActorKind::Infrastructure)
        ));
        assert!(credential_actor_kind("service".to_owned(), "human".to_owned()).is_err());
        assert!(browser_actor_kind("service".to_owned()).is_err());
    }

    #[tokio::test]
    async fn credential_touch_rejects_a_revoked_or_expired_row() {
        let (store, schema, bootstrap) = fixture().await;
        let principal = Uuid::now_v7();
        let credential = Uuid::now_v7();
        let identity = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO principals (id,kind,state,generation) VALUES ($1,'human','active',1)",
        )
        .bind(principal)
        .execute(store.pool())
        .await
        .expect("seed principal");
        sqlx::query("INSERT INTO oidc_identities (identity_id,issuer,subject,principal_id,link_generation) VALUES ($1,'issuer','revoked',$2,1)").bind(identity).bind(principal).execute(store.pool()).await.expect("seed identity");
        sqlx::query("INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,identity_id,digest_key_id,credential_kind,credential_generation,rotation_family_id,recovery_generation,expires_at,revoked_at) VALUES ($1,$2,decode(repeat('03',32),'hex'),$3,$4,'test','human',1,$1,1,clock_timestamp()+interval '1 hour',clock_timestamp())")
            .bind(credential)
            .bind(credential.to_string())
            .bind(principal)
            .bind(identity)
            .execute(store.pool())
            .await
            .expect("seed revoked credential");
        let mut transaction = store
            .begin_serializable()
            .await
            .expect("begin touch transaction");
        let error = touch_credential(&mut transaction, credential)
            .await
            .expect_err("revoked credential cannot be touched after lookup race");
        assert!(matches!(error, AuthError::CredentialInvalid));
        transaction
            .rollback()
            .await
            .expect("release touch transaction before schema cleanup");
        cleanup(&bootstrap, &schema).await;
    }

    #[tokio::test]
    async fn inactive_rotation_targets_are_rejected_without_durable_effects() {
        let (store, schema, bootstrap) = fixture_at_generation(2).await;
        let (authority, directory) = authority(store.clone()).await;
        let service = AuthService::new(
            store.clone(),
            DigestKey::new("test".into(), b"ephemeral-test-key".to_vec()),
            2,
            limits(),
            LoginPolicy::AnyAuthenticatedSubject,
            Arc::clone(&authority),
        );
        let principal_id = Uuid::now_v7();
        let identity_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO principals (id,kind,state,generation) VALUES ($1,'human','active',1)",
        )
        .bind(principal_id)
        .execute(store.pool())
        .await
        .expect("seed shared principal");
        sqlx::query("INSERT INTO oidc_identities (identity_id,issuer,subject,principal_id,link_generation) VALUES ($1,'issuer','inactive-rotation',$2,1)")
            .bind(identity_id)
            .bind(principal_id)
            .execute(store.pool())
            .await
            .expect("seed shared identity");
        let generation = 2_i64;
        for (label, state_sql) in [
            ("revoked", "revoked_at=clock_timestamp()"),
            (
                "expired",
                "issued_at=clock_timestamp()-interval '2 hours',expires_at=clock_timestamp()-interval '1 second'",
            ),
            ("stale", "recovery_generation=1"),
            (
                "elapsed",
                "rotation_overlap_ends_at=clock_timestamp()-interval '1 second'",
            ),
        ] {
            let actor_id = Uuid::now_v7();
            let target_id = Uuid::now_v7();
            let secret = format!("inactive-rotation-{label}");
            sqlx::query("INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,identity_id,digest_key_id,credential_kind,credential_generation,rotation_family_id,recovery_generation,expires_at) VALUES ($1,$2,$3,$4,$5,'test','human',1,$1,$6,clock_timestamp()+interval '1 hour')")
                .bind(actor_id).bind(actor_id.to_string()).bind(service.digest_key.digest(&secret).as_slice()).bind(principal_id).bind(identity_id).bind(generation)
                .execute(store.pool()).await.expect("seed current actor credential");
            let target_generation = if label == "stale" { 1_i64 } else { generation };
            sqlx::query("INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,identity_id,digest_key_id,credential_kind,credential_generation,rotation_family_id,recovery_generation,expires_at) VALUES ($1,$2,$3,$4,$5,'test','human',1,$1,$6,clock_timestamp()+interval '1 hour')")
                .bind(target_id).bind(target_id.to_string()).bind(service.digest_key.digest(&format!("inactive-target-{label}")).as_slice()).bind(principal_id).bind(identity_id).bind(target_generation)
                .execute(store.pool()).await.expect("seed inactive target");
            apply_restored_credential_change(&store, target_id, state_sql).await;
            let actor = service
                .authenticate_bearer(RelayBearerCredential::new(format!("{actor_id}.{secret}")))
                .await
                .expect("authenticate fresh actor");
            let request = RotateCredentialRequest {
                overlap_seconds: 1,
                idempotency: relay_protocol::Idempotency {
                    correlation_id: Uuid::now_v7(),
                    idempotency_key: Uuid::now_v7(),
                },
                expires_at: (time::OffsetDateTime::now_utc() + time::Duration::minutes(10))
                    .replace_nanosecond(0)
                    .expect("whole-second expiry"),
            };
            let credentials: i64 = sqlx::query_scalar("SELECT count(*) FROM relay_credentials")
                .fetch_one(store.pool())
                .await
                .expect("count credentials");
            let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
                .fetch_one(store.pool())
                .await
                .expect("count audits");
            let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM mutation_receipts")
                .fetch_one(store.pool())
                .await
                .expect("count receipts");
            let witness = WitnessStore::open(
                directory.path(),
                SigningKey::from_bytes(&[9; 32]),
                "test-key".into(),
            )
            .expect("reopen witness")
            .latest()
            .expect("read witness");
            let target_before: (i64, Option<time::OffsetDateTime>, time::OffsetDateTime, Option<time::OffsetDateTime>, i64) = sqlx::query_as("SELECT credential_generation,revoked_at,expires_at,rotation_overlap_ends_at,recovery_generation FROM relay_credentials WHERE credential_id=$1")
                .bind(target_id).fetch_one(store.pool()).await.expect("snapshot target metadata");
            for _ in 0..2 {
                assert!(matches!(
                    service
                        .rotate_credential(
                            actor,
                            CredentialId::from_uuid(target_id),
                            request.clone()
                        )
                        .await,
                    Err(AuthError::RotationRejected)
                ));
                let target_after: (i64, Option<time::OffsetDateTime>, time::OffsetDateTime, Option<time::OffsetDateTime>, i64) = sqlx::query_as("SELECT credential_generation,revoked_at,expires_at,rotation_overlap_ends_at,recovery_generation FROM relay_credentials WHERE credential_id=$1")
                    .bind(target_id).fetch_one(store.pool()).await.expect("read target metadata after rejection");
                assert_eq!(target_after, target_before, "{label} target remains exact");
            }
            assert_eq!(
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM relay_credentials")
                    .fetch_one(store.pool())
                    .await
                    .expect("count credentials after rejection"),
                credentials
            );
            assert_eq!(
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM audit_events")
                    .fetch_one(store.pool())
                    .await
                    .expect("count audits after rejection"),
                audits
            );
            assert_eq!(
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM mutation_receipts")
                    .fetch_one(store.pool())
                    .await
                    .expect("count receipts after rejection"),
                receipts
            );
            assert_eq!(
                WitnessStore::open(
                    directory.path(),
                    SigningKey::from_bytes(&[9; 32]),
                    "test-key".into()
                )
                .expect("reopen witness")
                .latest()
                .expect("read witness after rejection"),
                witness
            );
            assert!(
                !authority.is_closed(),
                "{label} rejection keeps authority live"
            );
        }
        cleanup(&bootstrap, &schema).await;
    }
}
