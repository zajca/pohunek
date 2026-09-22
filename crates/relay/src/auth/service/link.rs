//! Links an additional stable OIDC identity to an existing relay account.
//!
//! A link changes durable authority only when the current relay actor and the
//! new identity are both proven inside one audited transaction. The new identity
//! is proven with the same PKCE and device rules as login, reached through
//! [`AuthService::claim_and_poll_device`] and the shared browser callback
//! consumption, never a weaker copy. Issuer plus immutable subject is the whole
//! identity: no provider profile attribute reaches this module.

// Rust guideline compliant 2026-09-22

use std::time::Instant;

use relay_protocol::{
    AccountLinkChannel, AccountLinkPage, AccountLinkRecord, AccountLinkRequest, AccountLinkState,
    CancelAccountLinkRequest, Idempotency, IdentityRecord, IdentityRemoved, PageRequest,
    PrincipalId, UnlinkIdentityRequest,
};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

use crate::{
    admission::AuthMutationTarget,
    auth::{
        oidc::{OidcClient, OidcDeviceAuthorization, OidcIdentity},
        pending::PendingDeviceCode,
        AuthError, AuthenticatedActor, DevicePollSecret, SecretValue,
    },
    store::AuthenticationBinding,
};

use super::{
    audit_actor_mutation, database_error, exact_receipt, insert_receipt, interval, page_limit,
    random_secret, request_digest, retryable_database_error, AuthService, DeviceTransaction,
    PendingBrowserLogin, IDENTITY_ADVISORY_LOCK_SEED, IDENTITY_ISSUE_MAX_ATTEMPTS,
    RANDOM_SECRET_BYTES,
};

#[cfg(test)]
mod tests;

/// Bounds a link status page so one account cannot request unbounded history.
const MAX_LINK_PAGE: u16 = 128;
/// Audited action for starting a link transaction.
const LINK_BEGIN_ACTION: &str = "auth.link.begin";
/// Audited action for the durable identity-linking effect.
const LINK_COMPLETE_ACTION: &str = "auth.link.complete";
/// Audited action for cancelling a pending link transaction.
const LINK_CANCEL_ACTION: &str = "auth.link.cancel";
/// Audited action for removing a linked identity.
const UNLINK_ACTION: &str = "auth.identity.unlink";
/// Audited action for polling a device link transaction.
const LINK_POLL_ACTION: &str = "auth.link.poll";
/// `PostgreSQL` unique-violation code used to classify link collisions.
const UNIQUE_VIOLATION: &str = "23505";

/// Redirect target and one-use browser possession material for a link.
pub struct BrowserLinkStart {
    /// Safe revisioned view of the created transaction.
    pub record: AccountLinkRecord,
    /// Provider authorization URL the caller must open.
    pub authorization_url: url::Url,
    binding: SecretValue,
}

impl BrowserLinkStart {
    /// Borrows the one-use binding value for its protected response cookie.
    #[must_use]
    pub fn binding(&self) -> &str {
        self.binding.expose()
    }
}

impl std::fmt::Debug for BrowserLinkStart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserLinkStart")
            .field("record", &self.record)
            .field("redacted", &true)
            .finish_non_exhaustive()
    }
}

/// RFC 8628 values and one-use poll secret for a device link.
pub struct DeviceLinkStart {
    /// Safe revisioned view of the created transaction.
    pub record: AccountLinkRecord,
    /// Bounded provider device authorization shown to the terminal.
    pub authorization: OidcDeviceAuthorization,
    poll_secret: SecretValue,
}

impl DeviceLinkStart {
    /// Borrows the one-use poll secret for its protected response body.
    #[must_use]
    pub fn poll_secret(&self) -> &str {
        self.poll_secret.expose()
    }
}

impl std::fmt::Debug for DeviceLinkStart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceLinkStart")
            .field("record", &self.record)
            .field("redacted", &true)
            .finish_non_exhaustive()
    }
}

/// Identifies the proven linked identity and its account's new generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedIdentity {
    /// Safe revisioned view of the completed transaction.
    pub record: AccountLinkRecord,
    /// Stable coordinates of the identity that is now linked.
    pub identity: IdentityRecord,
}

/// Immutable coordinates of one locked pending link transaction.
#[derive(Debug, Clone, Copy)]
struct PendingLink {
    link_id: Uuid,
    principal_id: Uuid,
    account_link_generation: i64,
    recovery_generation: i64,
    revision: i64,
}

/// Binds one consumed browser callback to the link transaction it proves.
#[derive(Debug, Clone, Copy)]
pub(super) struct BrowserLinkBinding {
    pub(super) link_id: Uuid,
    pub(super) login_id: Uuid,
    pub(super) recovery_generation: i64,
    pub(super) account_link_generation: i64,
}

/// Describes one channel's provider transaction for a shared link commit.
#[derive(Debug, Clone, Copy)]
struct LinkCommit<'token> {
    link_id: Uuid,
    login_id: Uuid,
    channel: AccountLinkChannel,
    recovery_generation: i64,
    account_link_generation: i64,
    poll_lease_token: Option<&'token [u8; RANDOM_SECRET_BYTES]>,
}

impl AuthService {
    /// Starts a browser PKCE transaction proving a second identity for this account.
    ///
    /// # Errors
    /// Returns [`AuthError::LinkChannelMismatch`] for a bearer caller,
    /// [`AuthError::LinkPending`] when another transaction is already open, and
    /// [`AuthError::LinkQuarantined`] while the relay is in recovery quarantine.
    pub async fn begin_browser_link(
        &self,
        oidc: &OidcClient,
        actor: AuthenticatedActor,
        request: AccountLinkRequest,
    ) -> Result<BrowserLinkStart, AuthError> {
        let denied = LinkTarget::Transaction(None);
        if actor.actor().binding() != AuthenticationBinding::BrowserSession {
            return Err(self
                .deny(
                    actor,
                    LINK_BEGIN_ACTION,
                    denied,
                    AuthError::LinkChannelMismatch,
                )
                .await);
        }
        self.prune_expired_pending().await?;
        let reservation = match self.pending_transactions.reserve() {
            Ok(reservation) => reservation,
            Err(error) => return Err(self.deny(actor, LINK_BEGIN_ACTION, denied, error).await),
        };
        let authorization = oidc.begin_browser();
        let redirect_uri = oidc.redirect_uri().ok_or(AuthError::OidcInvalid)?;
        let binding = random_secret()?;
        let login_id = Uuid::now_v7();
        let mut transaction = match self.begin_link_transaction(actor).await {
            Ok(transaction) => transaction,
            Err(error) => return Err(self.deny(actor, LINK_BEGIN_ACTION, denied, error).await),
        };
        let source = match self.link_source(&mut transaction, actor).await {
            Ok(source) => source,
            Err(error) => {
                drop(transaction);
                return Err(self.deny(actor, LINK_BEGIN_ACTION, denied, error).await);
            }
        };
        let (source_identity_id, account_link_generation, recovery_generation) = source;
        // A retried start never mints a second provable transaction: the
        // duplicate is refused so the caller cancels before starting again.
        if let Err(error) = self
            .reject_duplicate_link(
                &mut transaction,
                actor,
                &request,
                AccountLinkChannel::Browser,
            )
            .await
        {
            drop(transaction);
            return Err(self.deny(actor, LINK_BEGIN_ACTION, denied, error).await);
        }
        let opened = self
            .open_link_transaction(
                &mut transaction,
                LinkInsert {
                    link_id: Uuid::now_v7(),
                    actor,
                    source_identity_id,
                    account_link_generation,
                    recovery_generation,
                    channel: AccountLinkChannel::Browser,
                    possession: &binding,
                    issuer: oidc.issuer(),
                    client_id: oidc.client_id(),
                    idempotency: request.idempotency,
                },
            )
            .await;
        let record = match opened {
            Ok(record) => record,
            Err(error) => {
                drop(transaction);
                return Err(self.deny(actor, LINK_BEGIN_ACTION, denied, error).await);
            }
        };
        let link_id = record.link_id;
        let inserted = sqlx::query(
            "INSERT INTO browser_logins (login_id, state_digest, nonce_digest, pkce_verifier_digest, login_binding_digest, issuer, client_id, audience, redirect_uri, action, link_id, account_link_generation, recovery_generation, expires_at) \
             SELECT $1, $2, $3, $4, $5, $6, $7, $7, $8, 'account_link', $9, $10, r.recovery_generation, clock_timestamp() + $11::interval \
             FROM relay_identity r WHERE r.state = 'normal' AND r.recovery_generation = $12",
        )
        .bind(login_id)
        .bind(self.digest_key.digest(&authorization.state).as_slice())
        .bind(self.digest_key.digest(&authorization.nonce).as_slice())
        .bind(self.digest_key.digest(&authorization.verifier).as_slice())
        .bind(self.digest_key.digest(&binding).as_slice())
        .bind(oidc.issuer())
        .bind(oidc.client_id())
        .bind(redirect_uri)
        .bind(link_id)
        .bind(record.account_link_generation)
        .bind(interval(self.limits.login_lifetime))
        .bind(recovery_generation)
        .execute(&mut *transaction)
        .await
        .map_err(link_database_error)?;
        if inserted.rows_affected() != 1 {
            drop(transaction);
            return Err(self
                .deny(actor, LINK_BEGIN_ACTION, denied, AuthError::LinkStale)
                .await);
        }
        audit_actor_mutation(
            &mut transaction,
            actor.actor(),
            LINK_BEGIN_ACTION,
            "account_link",
            record.link_id,
            None,
            request.idempotency,
            database_error,
        )
        .await?;
        self.commit_current(transaction).await?;
        let expires_at = Instant::now() + self.limits.login_lifetime;
        // The transaction and its provider row are already durable. Either
        // remaining step can fail, and the verifier and nonce live only in
        // process memory, so a failure here must close the transaction instead
        // of leaving an unprovable one that blocks every later start until it
        // expires.
        if let Err(error) = reservation.commit(login_id, expires_at) {
            self.cancel_link_after_start(actor, link_id, login_id)
                .await?;
            return Err(error);
        }
        let pending = PendingBrowserLogin {
            verifier: authorization.verifier,
            nonce: authorization.nonce,
            expires_at,
        };
        // The guard is released before the cleanup await so this future stays
        // `Send`.
        let stored = match self.pending_browser_logins.lock() {
            Ok(mut logins) => {
                logins.insert(login_id, pending);
                true
            }
            Err(_poisoned) => false,
        };
        if !stored {
            self.pending_transactions.release(login_id);
            self.cancel_link_after_start(actor, link_id, login_id)
                .await?;
            return Err(AuthError::Durable);
        }
        Ok(BrowserLinkStart {
            record,
            authorization_url: authorization.url,
            binding: SecretValue::new(binding),
        })
    }

    /// Starts a device transaction proving a second identity for this account.
    ///
    /// # Errors
    /// Returns [`AuthError::LinkChannelMismatch`] for a browser caller and
    /// [`AuthError::IssuerUnavailable`] when the configured issuer has no usable
    /// device endpoint.
    pub async fn begin_device_link(
        &self,
        oidc: &OidcClient,
        actor: AuthenticatedActor,
        request: AccountLinkRequest,
    ) -> Result<DeviceLinkStart, AuthError> {
        let denied = LinkTarget::Transaction(None);
        if actor.actor().binding() != AuthenticationBinding::Credential {
            return Err(self
                .deny(
                    actor,
                    LINK_BEGIN_ACTION,
                    denied,
                    AuthError::LinkChannelMismatch,
                )
                .await);
        }
        self.prune_expired_pending().await?;
        let reservation = match self.pending_transactions.reserve() {
            Ok(reservation) => reservation,
            Err(error) => return Err(self.deny(actor, LINK_BEGIN_ACTION, denied, error).await),
        };
        let authorization = oidc.begin_device().await?;
        let poll_secret = random_secret()?;
        let login_id = Uuid::now_v7();
        let mut transaction = match self.begin_link_transaction(actor).await {
            Ok(transaction) => transaction,
            Err(error) => return Err(self.deny(actor, LINK_BEGIN_ACTION, denied, error).await),
        };
        let source = match self.link_source(&mut transaction, actor).await {
            Ok(source) => source,
            Err(error) => {
                drop(transaction);
                return Err(self.deny(actor, LINK_BEGIN_ACTION, denied, error).await);
            }
        };
        let (source_identity_id, account_link_generation, recovery_generation) = source;
        // A retried start never mints a second provable transaction: the
        // duplicate is refused so the caller cancels before starting again.
        if let Err(error) = self
            .reject_duplicate_link(
                &mut transaction,
                actor,
                &request,
                AccountLinkChannel::Device,
            )
            .await
        {
            drop(transaction);
            return Err(self.deny(actor, LINK_BEGIN_ACTION, denied, error).await);
        }
        let opened = self
            .open_link_transaction(
                &mut transaction,
                LinkInsert {
                    link_id: Uuid::now_v7(),
                    actor,
                    source_identity_id,
                    account_link_generation,
                    recovery_generation,
                    channel: AccountLinkChannel::Device,
                    possession: &poll_secret,
                    issuer: oidc.issuer(),
                    client_id: oidc.client_id(),
                    idempotency: request.idempotency,
                },
            )
            .await;
        let record = match opened {
            Ok(record) => record,
            Err(error) => {
                drop(transaction);
                return Err(self.deny(actor, LINK_BEGIN_ACTION, denied, error).await);
            }
        };
        let link_id = record.link_id;
        let inserted = sqlx::query(
            "INSERT INTO device_logins (login_id, device_code_digest, poll_secret_digest, issuer, client_id, audience, action, link_id, account_link_generation, recovery_generation, expires_at, poll_interval_seconds, next_poll_at) \
             SELECT $1, $2, $3, $4, $5, $5, 'account_link', $6, $7, r.recovery_generation, clock_timestamp() + $8::interval, $9, clock_timestamp() \
             FROM relay_identity r WHERE r.state = 'normal' AND r.recovery_generation = $10",
        )
        .bind(login_id)
        .bind(self.digest_key.digest(authorization.device_code()).as_slice())
        .bind(self.digest_key.digest(&poll_secret).as_slice())
        .bind(oidc.issuer())
        .bind(oidc.client_id())
        .bind(link_id)
        .bind(record.account_link_generation)
        .bind(interval(authorization.expires_in))
        .bind(i32::try_from(authorization.interval.as_secs()).map_err(|_error| AuthError::Malformed)?)
        .bind(recovery_generation)
        .execute(&mut *transaction)
        .await
        .map_err(link_database_error)?;
        if inserted.rows_affected() != 1 {
            drop(transaction);
            return Err(self
                .deny(actor, LINK_BEGIN_ACTION, denied, AuthError::LinkStale)
                .await);
        }
        audit_actor_mutation(
            &mut transaction,
            actor.actor(),
            LINK_BEGIN_ACTION,
            "account_link",
            record.link_id,
            None,
            request.idempotency,
            database_error,
        )
        .await?;
        self.commit_current(transaction).await?;
        let expires_at = Instant::now() + authorization.expires_in;
        if let Err(error) = reservation.commit(login_id, expires_at) {
            self.cancel_link_after_start(actor, link_id, login_id)
                .await?;
            return Err(error);
        }
        if let Err(error) = self
            .pending_device_codes
            .insert(login_id, PendingDeviceCode::new(authorization.clone()))
        {
            self.pending_transactions.release(login_id);
            self.cancel_link_after_start(actor, link_id, login_id)
                .await?;
            return Err(error);
        }
        Ok(DeviceLinkStart {
            record,
            authorization,
            poll_secret: SecretValue::new(poll_secret),
        })
    }

    /// Polls one device link transaction owned by the calling credential.
    ///
    /// # Errors
    /// Returns the RFC 8628 retry classes from
    /// [`AuthService::poll_device_login`] plus the link-specific denials for a
    /// stale, cancelled, expired, replayed, or cross-principal transaction.
    pub async fn poll_device_link(
        &self,
        oidc: &OidcClient,
        actor: AuthenticatedActor,
        link_id: Uuid,
        secret: DevicePollSecret,
    ) -> Result<LinkedIdentity, AuthError> {
        let denied = LinkTarget::Transaction(Some(link_id));
        if actor.actor().binding() != AuthenticationBinding::Credential {
            return Err(self
                .deny(
                    actor,
                    LINK_POLL_ACTION,
                    denied,
                    AuthError::LinkChannelMismatch,
                )
                .await);
        }
        let mut transaction = match self.begin_link_transaction(actor).await {
            Ok(transaction) => transaction,
            Err(error) => return Err(self.deny(actor, LINK_POLL_ACTION, denied, error).await),
        };
        let pending = match self
            .locked_pending_link(&mut transaction, actor, link_id, AccountLinkChannel::Device)
            .await
        {
            Ok(pending) => pending,
            Err(error) => {
                drop(transaction);
                return Err(self.deny(actor, LINK_POLL_ACTION, denied, error).await);
            }
        };
        if !self.digest_key.matches(
            secret.expose(),
            possession_digest(&mut transaction, link_id)
                .await?
                .as_slice(),
        ) {
            drop(transaction);
            return Err(self
                .deny_with_outcome(
                    actor,
                    LINK_POLL_ACTION,
                    denied,
                    "possession_invalid",
                    AuthError::CredentialInvalid,
                )
                .await);
        }
        let login_id = sqlx::query_scalar::<_, Uuid>(
            "SELECT login_id FROM device_logins WHERE link_id = $1 AND action = 'account_link' \
             AND consumed_at IS NULL AND expires_at > clock_timestamp() FOR SHARE",
        )
        .bind(link_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?;
        self.commit_current(transaction).await?;
        let Some(login_id) = login_id else {
            return Err(self
                .deny(actor, LINK_POLL_ACTION, denied, AuthError::LinkExpired)
                .await);
        };
        let claim = self
            .claim_and_poll_device(
                oidc,
                login_id,
                &secret,
                DeviceTransaction::account_link(link_id, pending.account_link_generation),
            )
            .await?;
        let committed = self
            .commit_link(
                oidc,
                LinkCommit {
                    link_id,
                    login_id,
                    channel: AccountLinkChannel::Device,
                    recovery_generation: claim.recovery_generation,
                    account_link_generation: pending.account_link_generation,
                    poll_lease_token: Some(&claim.poll_lease_token),
                },
                &claim.identity,
                Some(actor),
            )
            .await;
        self.pending_transactions.release(login_id);
        committed
    }

    /// Commits one proven browser link after its callback was consumed.
    pub(super) async fn commit_browser_link(
        &self,
        oidc: &OidcClient,
        binding: BrowserLinkBinding,
        identity: &OidcIdentity,
        session: Option<AuthenticatedActor>,
    ) -> Result<AccountLinkRecord, AuthError> {
        self.commit_link(
            oidc,
            LinkCommit {
                link_id: binding.link_id,
                login_id: binding.login_id,
                channel: AccountLinkChannel::Browser,
                recovery_generation: binding.recovery_generation,
                account_link_generation: binding.account_link_generation,
                poll_lease_token: None,
            },
            identity,
            session,
        )
        .await
        .map(|linked| linked.record)
    }

    /// Lists the account's bounded link-transaction history.
    ///
    /// # Errors
    /// Returns [`AuthError::CredentialInvalid`] when the caller is no longer
    /// current and [`AuthError::Malformed`] for an out-of-range page limit.
    pub async fn list_account_links(
        &self,
        actor: AuthenticatedActor,
        page: PageRequest,
    ) -> Result<AccountLinkPage, AuthError> {
        if page.limit > MAX_LINK_PAGE {
            return Err(AuthError::Malformed);
        }
        let limit = page_limit(page.limit)?;
        let context = actor.actor();
        let mut transaction = self.begin_link_transaction(actor).await?;
        let mut records = sqlx::query(
            "SELECT link_id, principal_id, channel, state, source_identity_id, linked_identity_id, \
             revision, account_link_generation, created_at, expires_at, completed_at \
             FROM account_link_transactions WHERE principal_id = $1 \
             AND ($2::uuid IS NULL OR link_id > $2) ORDER BY link_id ASC LIMIT $3",
        )
        .bind(context.principal_id())
        .bind(page.after)
        .bind(limit + 1)
        .fetch_all(&mut *transaction)
        .await
        .map_err(database_error)?
        .into_iter()
        .map(|row| link_record(&row))
        .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = link_page_cursor(&mut records, limit)?;
        self.commit_current(transaction).await?;
        Ok(AccountLinkPage {
            records,
            next_cursor,
        })
    }

    /// Cancels one pending link transaction owned by this account.
    ///
    /// An already cancelled transaction returns its current record, so a retry
    /// after a lost response is safe.
    ///
    /// # Errors
    /// Returns [`AuthError::LinkReplayed`] for a completed transaction and
    /// [`AuthError::LinkNotFound`] when the account does not own it.
    pub async fn cancel_account_link(
        &self,
        actor: AuthenticatedActor,
        link_id: Uuid,
        request: CancelAccountLinkRequest,
    ) -> Result<AccountLinkRecord, AuthError> {
        let denied = LinkTarget::Transaction(Some(link_id));
        let mut transaction = match self.begin_link_transaction(actor).await {
            Ok(transaction) => transaction,
            Err(error) => return Err(self.deny(actor, LINK_CANCEL_ACTION, denied, error).await),
        };
        let found = sqlx::query(
            "SELECT link_id, principal_id, channel, state, source_identity_id, linked_identity_id, \
             revision, account_link_generation, created_at, expires_at, completed_at \
             FROM account_link_transactions WHERE link_id = $1 AND principal_id = $2 FOR UPDATE",
        )
        .bind(link_id)
        .bind(actor.actor().principal_id())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?;
        let Some(current) = found else {
            drop(transaction);
            return Err(self
                .deny(actor, LINK_CANCEL_ACTION, denied, AuthError::LinkNotFound)
                .await);
        };
        let current = link_record(&current)?;
        let refused = match current.state {
            AccountLinkState::Completed => Some(AuthError::LinkReplayed),
            AccountLinkState::Cancelled => {
                self.commit_current(transaction).await?;
                return Ok(current);
            }
            AccountLinkState::Expired => Some(AuthError::LinkExpired),
            AccountLinkState::Failed => Some(AuthError::LinkStale),
            AccountLinkState::Pending => None,
        };
        if let Some(error) = refused {
            drop(transaction);
            return Err(self.deny(actor, LINK_CANCEL_ACTION, denied, error).await);
        }
        let cancelled = sqlx::query(
            "UPDATE account_link_transactions SET state = 'cancelled', completed_at = clock_timestamp(), \
             revision = revision + 1 WHERE link_id = $1 AND principal_id = $2 AND state = 'pending' \
             RETURNING link_id, principal_id, channel, state, source_identity_id, linked_identity_id, \
             revision, account_link_generation, created_at, expires_at, completed_at",
        )
        .bind(link_id)
        .bind(actor.actor().principal_id())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?;
        let Some(cancelled) = cancelled else {
            drop(transaction);
            return Err(self
                .deny(actor, LINK_CANCEL_ACTION, denied, AuthError::LinkStale)
                .await);
        };
        let cancelled = link_record(&cancelled)?;
        self.supersede_link_logins(&mut transaction, link_id)
            .await?;
        audit_actor_mutation(
            &mut transaction,
            actor.actor(),
            LINK_CANCEL_ACTION,
            "account_link",
            link_id,
            None,
            request.idempotency,
            database_error,
        )
        .await?;
        self.commit_current(transaction).await?;
        Ok(cancelled)
    }

    /// Removes one linked identity and every authority derived from it.
    ///
    /// The identity's credentials and browser sessions are revoked, pending link
    /// transactions are cancelled, and the account generation advances, all in
    /// the same transaction as the audit record.
    ///
    /// # Errors
    /// Returns [`AuthError::UnlinkLastIdentity`] when the account would keep no
    /// identity, [`AuthError::IdentityNotFound`] for an unowned or already
    /// removed identity, and [`AuthError::Durable`] when the audit cannot commit.
    #[expect(
        clippy::too_many_lines,
        reason = "The unlink revocation, cancellation, audit, and witness commit are one fail-closed sequence."
    )]
    pub async fn unlink_identity(
        &self,
        actor: AuthenticatedActor,
        identity_id: Uuid,
        request: UnlinkIdentityRequest,
    ) -> Result<IdentityRemoved, AuthError> {
        let context = actor.actor();
        let denied = LinkTarget::Identity(identity_id);
        let mut transaction = match self.begin_link_transaction(actor).await {
            Ok(transaction) => transaction,
            Err(error) => return Err(self.deny(actor, UNLINK_ACTION, denied, error).await),
        };
        let digest = request_digest(&self.digest_key, &(identity_id, request))?;
        if exact_receipt(
            &mut transaction,
            context,
            UNLINK_ACTION,
            request.idempotency.idempotency_key,
            &digest,
        )
        .await?
        .is_some()
        {
            let replay = removed_identity(&mut transaction, context.principal_id(), identity_id)
                .await?
                .ok_or(AuthError::Durable)?;
            self.authority
                .commit_auth_mutation(transaction, false, None)
                .await
                .map_err(|_error| AuthError::Durable)?;
            return Ok(replay);
        }
        let generation = sqlx::query_scalar::<_, i64>(
            "SELECT account_link_generation FROM principals WHERE id = $1 AND state = 'active' FOR UPDATE",
        )
        .bind(context.principal_id())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(retryable_database_error)?;
        let Some(generation) = generation else {
            drop(transaction);
            return Err(self
                .deny(actor, UNLINK_ACTION, denied, AuthError::CredentialInvalid)
                .await);
        };
        let active: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM oidc_identities WHERE principal_id = $1 AND removed_at IS NULL",
        )
        .bind(context.principal_id())
        .fetch_one(&mut *transaction)
        .await
        .map_err(retryable_database_error)?;
        // An account with no identity could never authenticate again, so its
        // last identity is removed only by deprovisioning the principal.
        if active <= 1 {
            drop(transaction);
            return Err(self
                .deny(actor, UNLINK_ACTION, denied, AuthError::UnlinkLastIdentity)
                .await);
        }
        let removed = sqlx::query(
            "UPDATE oidc_identities SET removed_at = clock_timestamp(), \
             link_generation = link_generation + 1 \
             WHERE identity_id = $1 AND principal_id = $2 AND removed_at IS NULL RETURNING removed_at",
        )
        .bind(identity_id)
        .bind(context.principal_id())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(retryable_database_error)?;
        let Some(removed) = removed else {
            drop(transaction);
            return Err(self
                .deny(actor, UNLINK_ACTION, denied, AuthError::IdentityNotFound)
                .await);
        };
        let removed_at: time::OffsetDateTime = removed.get("removed_at");
        let revoked_credentials = sqlx::query(
            "UPDATE relay_credentials SET revoked_at = clock_timestamp(), \
             credential_generation = credential_generation + 1 \
             WHERE identity_id = $1 AND principal_id = $2 AND revoked_at IS NULL",
        )
        .bind(identity_id)
        .bind(context.principal_id())
        .execute(&mut *transaction)
        .await
        .map_err(retryable_database_error)?
        .rows_affected();
        let revoked_sessions = sqlx::query(
            "UPDATE browser_sessions SET revoked_at = clock_timestamp(), \
             session_generation = session_generation + 1 \
             WHERE identity_id = $1 AND principal_id = $2 AND revoked_at IS NULL",
        )
        .bind(identity_id)
        .bind(context.principal_id())
        .execute(&mut *transaction)
        .await
        .map_err(retryable_database_error)?
        .rows_affected();
        let stale = sqlx::query_scalar::<_, Uuid>(
            "UPDATE account_link_transactions SET state = 'cancelled', \
             completed_at = clock_timestamp(), revision = revision + 1 \
             WHERE principal_id = $1 AND state = 'pending' RETURNING link_id",
        )
        .bind(context.principal_id())
        .fetch_all(&mut *transaction)
        .await
        .map_err(retryable_database_error)?;
        for link_id in stale {
            self.supersede_link_logins(&mut transaction, link_id)
                .await?;
        }
        let advanced = sqlx::query_scalar::<_, i64>(
            "UPDATE principals SET account_link_generation = account_link_generation + 1, \
             generation = generation + 1 WHERE id = $1 AND account_link_generation = $2 \
             RETURNING account_link_generation",
        )
        .bind(context.principal_id())
        .bind(generation)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(retryable_database_error)?;
        let Some(advanced) = advanced else {
            // The removal and its revocations roll back with this transaction,
            // so the denial is recorded on its own.
            drop(transaction);
            return Err(self
                .deny(actor, UNLINK_ACTION, denied, AuthError::LinkStale)
                .await);
        };
        let audit_id = match audit_actor_mutation(
            &mut transaction,
            context,
            UNLINK_ACTION,
            "oidc_identity",
            identity_id,
            None,
            request.idempotency,
            database_error,
        )
        .await
        {
            Ok(audit_id) => audit_id,
            Err(_error) => {
                // Close active access before the durable removal rolls back so a
                // missing audit can never leave the removed identity usable.
                let _ = self
                    .authority
                    .fail_stop_auth_revocation(AuthMutationTarget::Principal(
                        context.principal_id(),
                    ));
                return Err(AuthError::Durable);
            }
        };
        insert_receipt(
            &mut transaction,
            context,
            UNLINK_ACTION,
            request.idempotency.idempotency_key,
            &digest,
            audit_id,
            identity_id,
        )
        .await?;
        self.authority
            .commit_auth_mutation(
                transaction,
                true,
                Some(AuthMutationTarget::Principal(context.principal_id())),
            )
            .await
            .map_err(|_error| AuthError::Durable)?;
        Ok(IdentityRemoved {
            identity_id,
            principal_id: PrincipalId::from_uuid(context.principal_id()),
            removed_at,
            account_link_generation: advanced,
            revoked_credentials: u32::try_from(revoked_credentials)
                .map_err(|_error| AuthError::Durable)?,
            revoked_sessions: u32::try_from(revoked_sessions)
                .map_err(|_error| AuthError::Durable)?,
        })
    }

    /// Records one attributable denied link transition in its own transaction.
    ///
    /// A refused attempt changes nothing else, so the audit is its only durable
    /// effect and must not roll back with the work it refused. Callers drop
    /// their own transaction first, so no partial write is ever committed
    /// alongside the denial. The returned error is the caller's original
    /// refusal unless the audit itself could not be written, which fails closed.
    async fn deny(
        &self,
        actor: AuthenticatedActor,
        action: &'static str,
        target: LinkTarget,
        error: AuthError,
    ) -> AuthError {
        let outcome = denial_outcome(&error);
        self.deny_with_outcome(actor, action, target, outcome, error)
            .await
    }

    /// Records one denied link transition under an explicit outcome code.
    ///
    /// Used where the cause is narrower than the typed error can express, such
    /// as a failed possession check that surfaces as an invalid credential.
    async fn deny_with_outcome(
        &self,
        actor: AuthenticatedActor,
        action: &'static str,
        target: LinkTarget,
        outcome: &'static str,
        error: AuthError,
    ) -> AuthError {
        let Ok(mut transaction) = self.store.begin_serializable().await else {
            return AuthError::Durable;
        };
        if let Err(durable) =
            audit_link_denial(&mut transaction, actor.actor(), action, target, outcome).await
        {
            return durable;
        }
        match transaction.commit().await {
            Ok(()) => error,
            Err(_error) => AuthError::Durable,
        }
    }

    /// Opens a fence-verified transaction with a current, non-quarantined actor.
    async fn begin_link_transaction(
        &self,
        actor: AuthenticatedActor,
    ) -> Result<Transaction<'_, Postgres>, AuthError> {
        let mut transaction = self
            .store
            .begin_serializable()
            .await
            .map_err(|_error| AuthError::Durable)?;
        self.authority
            .verify_fence_in_transaction(&mut transaction)
            .await
            .map_err(|_error| AuthError::Durable)?;
        let generation = sqlx::query_scalar::<_, i64>(
            "SELECT recovery_generation FROM relay_identity WHERE state = 'normal' FOR SHARE",
        )
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?
        .ok_or(AuthError::LinkQuarantined)?;
        if generation != actor.actor().recovery_generation() {
            return Err(AuthError::LinkQuarantined);
        }
        crate::authorization::verify_current_actor(&mut transaction, actor.actor())
            .await
            .map_err(|_error| AuthError::LinkStale)?;
        Ok(transaction)
    }

    /// Resolves the source identity, account generation, and recovery generation.
    async fn link_source(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        actor: AuthenticatedActor,
    ) -> Result<(Uuid, i64, i64), AuthError> {
        let source_identity_id = actor.identity_id().ok_or(AuthError::LinkUnsupportedActor)?;
        let row = sqlx::query(
            "SELECT p.kind, p.account_link_generation, r.recovery_generation \
             FROM principals p JOIN relay_identity r ON r.state = 'normal' \
             WHERE p.id = $1 AND p.state = 'active' FOR UPDATE OF p",
        )
        .bind(actor.actor().principal_id())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(retryable_database_error)?
        .ok_or(AuthError::CredentialInvalid)?;
        // Only a human or infrastructure principal has OIDC identities; a
        // service account never links one.
        if !matches!(
            row.get::<String, _>("kind").as_str(),
            "human" | "infrastructure"
        ) {
            return Err(AuthError::LinkUnsupportedActor);
        }
        let present = sqlx::query_scalar::<_, Uuid>(
            "SELECT identity_id FROM oidc_identities WHERE identity_id = $1 AND principal_id = $2 \
             AND removed_at IS NULL FOR SHARE",
        )
        .bind(source_identity_id)
        .bind(actor.actor().principal_id())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(retryable_database_error)?;
        if present.is_none() {
            return Err(AuthError::LinkStale);
        }
        Ok((
            source_identity_id,
            row.get("account_link_generation"),
            row.get("recovery_generation"),
        ))
    }

    /// Refuses a start whose retry coordinate already owns a transaction.
    ///
    /// Possession material is delivered once, so re-arming a transaction is not
    /// possible; the caller cancels the open transaction and starts a new one.
    async fn reject_duplicate_link(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        actor: AuthenticatedActor,
        request: &AccountLinkRequest,
        channel: AccountLinkChannel,
    ) -> Result<(), AuthError> {
        let existing = sqlx::query(
            "SELECT channel, state, correlation_id FROM account_link_transactions \
             WHERE principal_id = $1 AND idempotency_key = $2 FOR SHARE",
        )
        .bind(actor.actor().principal_id())
        .bind(request.idempotency.idempotency_key)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(retryable_database_error)?;
        let Some(existing) = existing else {
            return Ok(());
        };
        if existing.get::<String, _>("channel") != channel_name(channel)
            || existing.get::<Uuid, _>("correlation_id") != request.idempotency.correlation_id
        {
            return Err(AuthError::IdempotencyConflict);
        }
        if existing.get::<String, _>("state") == "pending" {
            Err(AuthError::LinkPending)
        } else {
            Err(AuthError::LinkReplayed)
        }
    }

    /// Opens one pending link transaction bound to every current coordinate.
    async fn open_link_transaction(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        insert: LinkInsert<'_>,
    ) -> Result<AccountLinkRecord, AuthError> {
        let row = sqlx::query(
                "INSERT INTO account_link_transactions (link_id, principal_id, source_identity_id, channel, \
                 source_authentication_id, source_authentication_generation, issuer, client_id, audience, \
                 possession_digest, digest_key_id, state, revision, correlation_id, idempotency_key, \
                 account_link_generation, recovery_generation, expires_at) \
                 SELECT $1, $2, $3, $4, $5, $6, $7, $8, $8, $9, $10, 'pending', 1, $11, $12, $13, \
                 r.recovery_generation, clock_timestamp() + $14::interval \
                 FROM relay_identity r WHERE r.state = 'normal' AND r.recovery_generation = $15 \
                 RETURNING link_id, principal_id, channel, state, source_identity_id, \
                 linked_identity_id, revision, account_link_generation, created_at, expires_at, completed_at",
            )
            .bind(insert.link_id)
            .bind(insert.actor.actor().principal_id())
            .bind(insert.source_identity_id)
            .bind(channel_name(insert.channel))
            .bind(insert.actor.actor().authentication_id())
            .bind(insert.actor.actor().authentication_generation())
            .bind(insert.issuer)
            .bind(insert.client_id)
            .bind(self.digest_key.digest(insert.possession).as_slice())
            .bind(self.digest_key.key_id())
            .bind(insert.idempotency.correlation_id)
            .bind(insert.idempotency.idempotency_key)
            .bind(insert.account_link_generation)
            .bind(interval(self.limits.login_lifetime))
            .bind(insert.recovery_generation)
            .fetch_optional(&mut **transaction)
            .await;
        link_record(
            &row.map_err(link_database_error)?
                .ok_or(AuthError::LinkStale)?,
        )
    }

    /// Closes any unconsumed provider row bound to one link transaction.
    async fn supersede_link_logins(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        link_id: Uuid,
    ) -> Result<(), AuthError> {
        let superseded = sqlx::query_scalar::<_, Uuid>(
            "UPDATE browser_logins SET consumed_at = clock_timestamp(), outcome = 'cancelled' \
             WHERE link_id = $1 AND consumed_at IS NULL RETURNING login_id",
        )
        .bind(link_id)
        .fetch_all(&mut **transaction)
        .await
        .map_err(retryable_database_error)?;
        let mut released = superseded;
        released.extend(
            sqlx::query_scalar::<_, Uuid>(
                "UPDATE device_logins SET consumed_at = clock_timestamp(), outcome = 'cancelled', \
                 poll_lease_until = NULL, poll_lease_token = NULL \
                 WHERE link_id = $1 AND consumed_at IS NULL RETURNING login_id",
            )
            .bind(link_id)
            .fetch_all(&mut **transaction)
            .await
            .map_err(retryable_database_error)?,
        );
        for login_id in released {
            self.remove_pending_browser_login(login_id);
            self.pending_device_codes.take(login_id);
            self.pending_transactions.release(login_id);
        }
        Ok(())
    }

    /// Cancels a started link whose process-local admission could not complete.
    async fn cancel_link_after_start(
        &self,
        actor: AuthenticatedActor,
        link_id: Uuid,
        login_id: Uuid,
    ) -> Result<(), AuthError> {
        let mut transaction = self
            .store
            .begin_serializable()
            .await
            .map_err(|_error| AuthError::Durable)?;
        self.authority
            .verify_fence_in_transaction(&mut transaction)
            .await
            .map_err(|_error| AuthError::Durable)?;
        sqlx::query(
            "UPDATE account_link_transactions SET state = 'cancelled', \
             completed_at = clock_timestamp(), revision = revision + 1 \
             WHERE link_id = $1 AND state = 'pending'",
        )
        .bind(link_id)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        self.supersede_link_logins(&mut transaction, link_id)
            .await?;
        audit_link_denial(
            &mut transaction,
            actor.actor(),
            LINK_BEGIN_ACTION,
            LinkTarget::Transaction(Some(link_id)),
            "start_incomplete",
        )
        .await?;
        self.commit_current(transaction).await?;
        self.pending_transactions.release(login_id);
        Ok(())
    }

    /// Locks one pending link transaction and checks every current binding.
    async fn locked_pending_link(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        actor: AuthenticatedActor,
        link_id: Uuid,
        channel: AccountLinkChannel,
    ) -> Result<PendingLink, AuthError> {
        let context = actor.actor();
        let row = sqlx::query(
            "SELECT t.link_id, t.principal_id, t.source_identity_id, t.channel, t.state, \
             t.source_authentication_id, t.source_authentication_generation, t.account_link_generation, \
             t.recovery_generation, t.revision, t.expires_at <= clock_timestamp() AS is_expired, \
             p.account_link_generation AS current_generation \
             FROM account_link_transactions t JOIN principals p ON p.id = t.principal_id \
             WHERE t.link_id = $1 FOR UPDATE OF t",
        )
        .bind(link_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(retryable_database_error)?
        .ok_or(AuthError::LinkNotFound)?;
        if row.get::<Uuid, _>("principal_id") != context.principal_id() {
            return Err(AuthError::LinkCrossPrincipal);
        }
        match row.get::<String, _>("state").as_str() {
            "pending" => {}
            "completed" => return Err(AuthError::LinkReplayed),
            "cancelled" => return Err(AuthError::LinkCancelled),
            "expired" => return Err(AuthError::LinkExpired),
            _ => return Err(AuthError::LinkStale),
        }
        if row.get::<bool, _>("is_expired") {
            return Err(AuthError::LinkExpired);
        }
        if row.get::<String, _>("channel") != channel_name(channel) {
            return Err(AuthError::LinkChannelMismatch);
        }
        if row.get::<Uuid, _>("source_authentication_id") != context.authentication_id()
            || row.get::<i64, _>("source_authentication_generation")
                != context.authentication_generation()
            || actor.identity_id() != Some(row.get("source_identity_id"))
            || row.get::<i64, _>("recovery_generation") != context.recovery_generation()
            || row.get::<i64, _>("account_link_generation")
                != row.get::<i64, _>("current_generation")
        {
            return Err(AuthError::LinkStale);
        }
        Ok(PendingLink {
            link_id: row.get("link_id"),
            principal_id: row.get("principal_id"),
            account_link_generation: row.get("account_link_generation"),
            recovery_generation: row.get("recovery_generation"),
            revision: row.get("revision"),
        })
    }

    /// Commits one proven identity into the account, retrying serialization conflicts.
    async fn commit_link(
        &self,
        oidc: &OidcClient,
        commit: LinkCommit<'_>,
        identity: &OidcIdentity,
        session: Option<AuthenticatedActor>,
    ) -> Result<LinkedIdentity, AuthError> {
        for _attempt in 0..IDENTITY_ISSUE_MAX_ATTEMPTS {
            match self.commit_link_once(oidc, commit, identity, session).await {
                Err(AuthError::Retryable) => {}
                result => return result,
            }
        }
        Err(AuthError::Durable)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "The link proof, collision checks, generation advance, and audit are one reviewed transaction."
    )]
    async fn commit_link_once(
        &self,
        oidc: &OidcClient,
        commit: LinkCommit<'_>,
        identity: &OidcIdentity,
        session: Option<AuthenticatedActor>,
    ) -> Result<LinkedIdentity, AuthError> {
        let actor = session.ok_or(AuthError::LinkChannelMismatch)?;
        let expected_binding = match commit.channel {
            AccountLinkChannel::Browser => AuthenticationBinding::BrowserSession,
            AccountLinkChannel::Device => AuthenticationBinding::Credential,
        };
        let denied = LinkTarget::Transaction(Some(commit.link_id));
        if actor.actor().binding() != expected_binding {
            return Err(self
                .deny(
                    actor,
                    LINK_COMPLETE_ACTION,
                    denied,
                    AuthError::LinkChannelMismatch,
                )
                .await);
        }
        let mut transaction = match self.begin_link_transaction(actor).await {
            Ok(transaction) => transaction,
            Err(error) => return Err(self.deny(actor, LINK_COMPLETE_ACTION, denied, error).await),
        };
        let pending = match self
            .locked_pending_link(&mut transaction, actor, commit.link_id, commit.channel)
            .await
        {
            Ok(pending) => pending,
            Err(error) => {
                drop(transaction);
                return Err(self.deny(actor, LINK_COMPLETE_ACTION, denied, error).await);
            }
        };
        if pending.recovery_generation != commit.recovery_generation
            || pending.account_link_generation != commit.account_link_generation
        {
            drop(transaction);
            return Err(self
                .deny_with_outcome(
                    actor,
                    LINK_COMPLETE_ACTION,
                    denied,
                    "generation_stale",
                    AuthError::LinkStale,
                )
                .await);
        }
        let bound = sqlx::query_scalar::<_, Uuid>(
            "SELECT link_id FROM account_link_transactions WHERE link_id = $1 AND issuer = $2 \
             AND client_id = $3 AND audience = $3 AND digest_key_id = $4 FOR SHARE",
        )
        .bind(commit.link_id)
        .bind(&identity.issuer)
        .bind(oidc.client_id())
        .bind(self.digest_key.key_id())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(retryable_database_error)?;
        if bound.is_none() {
            drop(transaction);
            return Err(self
                .deny_with_outcome(
                    actor,
                    LINK_COMPLETE_ACTION,
                    denied,
                    "binding_mismatch",
                    AuthError::LinkStale,
                )
                .await);
        }
        if !self
            .consume_link_login(&mut transaction, commit, pending)
            .await?
        {
            drop(transaction);
            return Err(self
                .deny_with_outcome(
                    actor,
                    LINK_COMPLETE_ACTION,
                    denied,
                    "proof_unconsumable",
                    AuthError::LinkStale,
                )
                .await);
        }
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, $2))")
            .bind(format!("{}\u{1f}{}", identity.issuer, identity.subject))
            .bind(IDENTITY_ADVISORY_LOCK_SEED)
            .execute(&mut *transaction)
            .await
            .map_err(retryable_database_error)?;
        let existing = sqlx::query_scalar::<_, Uuid>(
            "SELECT principal_id FROM oidc_identities WHERE issuer = $1 AND subject = $2 \
             AND removed_at IS NULL FOR UPDATE",
        )
        .bind(&identity.issuer)
        .bind(&identity.subject)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(retryable_database_error)?;
        if let Some(owner) = existing {
            let (outcome, error) = if owner == pending.principal_id {
                ("self_link", AuthError::LinkSelf)
            } else {
                ("collision", AuthError::LinkCollision)
            };
            self.fail_link(&mut transaction, actor, pending, outcome)
                .await?;
            self.commit_current(transaction).await?;
            return Err(error);
        }
        let identity_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO oidc_identities (identity_id, issuer, subject, principal_id, \
             link_generation, linked_via_link_id) VALUES ($1, $2, $3, $4, 1, $5)",
        )
        .bind(identity_id)
        .bind(&identity.issuer)
        .bind(&identity.subject)
        .bind(pending.principal_id)
        .bind(commit.link_id)
        .execute(&mut *transaction)
        .await
        .map_err(link_identity_error)?;
        let advanced = sqlx::query_scalar::<_, i64>(
            "UPDATE principals SET account_link_generation = account_link_generation + 1 \
             WHERE id = $1 AND account_link_generation = $2 RETURNING account_link_generation",
        )
        .bind(pending.principal_id)
        .bind(pending.account_link_generation)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(retryable_database_error)?;
        if advanced.is_none() {
            drop(transaction);
            return Err(self
                .deny_with_outcome(
                    actor,
                    LINK_COMPLETE_ACTION,
                    denied,
                    "generation_advance_lost",
                    AuthError::LinkStale,
                )
                .await);
        }
        let completed = sqlx::query(
            "UPDATE account_link_transactions SET state = 'completed', \
             completed_at = clock_timestamp(), linked_identity_id = $3, revision = revision + 1 \
             WHERE link_id = $1 AND principal_id = $2 AND state = 'pending' AND revision = $4 \
             RETURNING link_id, principal_id, channel, state, source_identity_id, linked_identity_id, \
             revision, account_link_generation, created_at, expires_at, completed_at",
        )
        .bind(commit.link_id)
        .bind(pending.principal_id)
        .bind(identity_id)
        .bind(pending.revision)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(retryable_database_error)?;
        let Some(completed) = completed else {
            drop(transaction);
            return Err(self
                .deny_with_outcome(
                    actor,
                    LINK_COMPLETE_ACTION,
                    denied,
                    "completion_lost",
                    AuthError::LinkStale,
                )
                .await);
        };
        let record = link_record(&completed)?;
        let idempotency = link_idempotency(&mut transaction, commit.link_id).await?;
        audit_actor_mutation(
            &mut transaction,
            actor.actor(),
            LINK_COMPLETE_ACTION,
            "oidc_identity",
            identity_id,
            None,
            idempotency,
            retryable_database_error,
        )
        .await?;
        self.commit_current(transaction).await?;
        Ok(LinkedIdentity {
            record,
            identity: IdentityRecord {
                identity_id,
                issuer: identity.issuer.clone(),
                subject: identity.subject.clone(),
            },
        })
    }

    /// Consumes the channel's provider row exactly once for this link.
    async fn consume_link_login(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        commit: LinkCommit<'_>,
        pending: PendingLink,
    ) -> Result<bool, AuthError> {
        let consumed = match commit.channel {
            AccountLinkChannel::Browser => sqlx::query(
                "UPDATE browser_logins SET outcome = 'succeeded' WHERE login_id = $1 \
                 AND link_id = $2 AND action = 'account_link' AND account_link_generation = $3 \
                 AND consumed_at IS NOT NULL AND outcome = 'failed' AND expires_at > clock_timestamp() \
                 AND recovery_generation = $4 RETURNING login_id",
            )
            .bind(commit.login_id)
            .bind(commit.link_id)
            .bind(pending.account_link_generation)
            .bind(pending.recovery_generation)
            .fetch_optional(&mut **transaction)
            .await
            .map_err(retryable_database_error)?,
            AccountLinkChannel::Device => {
                let token = commit.poll_lease_token.ok_or(AuthError::LinkStale)?;
                sqlx::query(
                    "UPDATE device_logins SET consumed_at = clock_timestamp(), outcome = 'succeeded', \
                     poll_lease_until = NULL, poll_lease_token = NULL WHERE login_id = $1 \
                     AND link_id = $2 AND action = 'account_link' AND account_link_generation = $3 \
                     AND recovery_generation = $4 AND poll_lease_token = $5 \
                     AND poll_lease_until > clock_timestamp() AND consumed_at IS NULL \
                     AND expires_at > clock_timestamp() RETURNING login_id",
                )
                .bind(commit.login_id)
                .bind(commit.link_id)
                .bind(pending.account_link_generation)
                .bind(pending.recovery_generation)
                .bind(token.as_slice())
                .fetch_optional(&mut **transaction)
                .await
                .map_err(retryable_database_error)?
            }
        };
        Ok(consumed.is_some())
    }

    /// Records one denied link attempt and closes its transaction.
    async fn fail_link(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
        actor: AuthenticatedActor,
        pending: PendingLink,
        outcome: &'static str,
    ) -> Result<(), AuthError> {
        sqlx::query(
            "UPDATE account_link_transactions SET state = 'failed', \
             completed_at = clock_timestamp(), revision = revision + 1 \
             WHERE link_id = $1 AND state = 'pending' AND revision = $2",
        )
        .bind(pending.link_id)
        .bind(pending.revision)
        .execute(&mut **transaction)
        .await
        .map_err(retryable_database_error)?;
        audit_link_denial(
            transaction,
            actor.actor(),
            LINK_COMPLETE_ACTION,
            LinkTarget::Transaction(Some(pending.link_id)),
            outcome,
        )
        .await
    }
}

/// Carries the bounded coordinates of one new link transaction.
#[derive(Debug, Clone, Copy)]
struct LinkInsert<'value> {
    link_id: Uuid,
    actor: AuthenticatedActor,
    source_identity_id: Uuid,
    account_link_generation: i64,
    recovery_generation: i64,
    channel: AccountLinkChannel,
    possession: &'value str,
    issuer: &'value str,
    client_id: &'value str,
    idempotency: Idempotency,
}

/// Names the durable resource a denied link transition was aimed at.
#[derive(Debug, Clone, Copy)]
enum LinkTarget {
    /// A link transaction coordinate, or none when the denial precedes one.
    Transaction(Option<Uuid>),
    /// A linked identity coordinate.
    Identity(Uuid),
}

impl LinkTarget {
    const fn parameter_code(self) -> &'static str {
        match self {
            Self::Transaction(_) => "account_link",
            Self::Identity(_) => "oidc_identity",
        }
    }

    fn parameter_value(self) -> String {
        match self {
            Self::Transaction(Some(id)) | Self::Identity(id) => id.to_string(),
            // A denial that never reached a transaction still records the
            // attempt; only its target coordinate is absent.
            Self::Transaction(None) => "none".to_owned(),
        }
    }
}

/// Returns the safe, distinct outcome code for one denied link transition.
///
/// Several denials share a typed error because the public contract is coarser
/// than the cause. The audit record is the evidence surface, so it keeps the
/// causes apart.
const fn denial_outcome(error: &AuthError) -> &'static str {
    match error {
        AuthError::LinkChannelMismatch => "channel_mismatch",
        AuthError::LinkQuarantined => "recovery_quarantine",
        AuthError::LinkUnsupportedActor => "unsupported_actor",
        AuthError::LinkPending => "already_pending",
        AuthError::LinkReplayed => "replayed",
        AuthError::LinkCancelled => "cancelled",
        AuthError::LinkExpired => "expired",
        AuthError::LinkNotFound => "not_found",
        AuthError::LinkCrossPrincipal => "cross_principal",
        AuthError::LinkSelf => "self_link",
        AuthError::LinkCollision => "collision",
        AuthError::LinkStale => "stale",
        AuthError::IdempotencyConflict => "idempotency_conflict",
        AuthError::UnlinkLastIdentity => "last_identity",
        AuthError::IdentityNotFound => "identity_not_found",
        AuthError::CredentialInvalid => "actor_invalid",
        AuthError::Capacity => "capacity",
        _ => "denied",
    }
}

/// Records one attributable denied link transition.
///
/// Unlike an allowed transition this carries no idempotency key, because a
/// denial is not a retryable effect. It deliberately does not require the relay
/// to be in `normal` state, so a recovery-quarantine denial is still recorded.
///
/// # Errors
/// Returns [`AuthError::Durable`] when the row cannot be written, so a denial
/// that cannot be evidenced fails closed rather than being silently dropped.
async fn audit_link_denial(
    transaction: &mut Transaction<'_, Postgres>,
    actor: crate::store::ActorContext,
    action: &'static str,
    target: LinkTarget,
    outcome: &'static str,
) -> Result<(), AuthError> {
    let inserted = sqlx::query(
        "INSERT INTO audit_events (audit_id, actor_principal_id, actor_kind, team_id, action, decision, policy_generation, recovery_generation, correlation_id, parameter_code, parameter_value, outcome) \
         SELECT $1, $2, $3, NULL, $4, 'deny', r.revision, r.recovery_generation, $5, $6, $7, $8 \
         FROM relay_identity r",
    )
    .bind(Uuid::now_v7())
    .bind(actor.principal_id())
    .bind(crate::store::actor_kind_name(actor.kind()))
    .bind(action)
    .bind(Uuid::now_v7())
    .bind(target.parameter_code())
    .bind(target.parameter_value())
    .bind(outcome)
    .execute(&mut **transaction)
    .await
    .map_err(retryable_database_error)?;
    if inserted.rows_affected() != 1 {
        return Err(AuthError::Durable);
    }
    Ok(())
}

/// Returns the durable name of one possession channel.
const fn channel_name(channel: AccountLinkChannel) -> &'static str {
    match channel {
        AccountLinkChannel::Browser => "browser",
        AccountLinkChannel::Device => "device",
    }
}

/// Reads the keyed possession digest bound to one link transaction.
async fn possession_digest(
    transaction: &mut Transaction<'_, Postgres>,
    link_id: Uuid,
) -> Result<Vec<u8>, AuthError> {
    sqlx::query_scalar::<_, Vec<u8>>(
        "SELECT possession_digest FROM account_link_transactions WHERE link_id = $1 FOR SHARE",
    )
    .bind(link_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(database_error)?
    .ok_or(AuthError::LinkNotFound)
}

/// Rebuilds the transaction's stored retry coordinate for its audit record.
async fn link_idempotency(
    transaction: &mut Transaction<'_, Postgres>,
    link_id: Uuid,
) -> Result<Idempotency, AuthError> {
    let row = sqlx::query(
        "SELECT correlation_id, idempotency_key FROM account_link_transactions \
         WHERE link_id = $1 FOR SHARE",
    )
    .bind(link_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(retryable_database_error)?
    .ok_or(AuthError::LinkStale)?;
    Ok(Idempotency {
        correlation_id: row.get("correlation_id"),
        idempotency_key: row.get("idempotency_key"),
    })
}

/// Reads the already-committed removal result for an exact unlink retry.
async fn removed_identity(
    transaction: &mut Transaction<'_, Postgres>,
    principal_id: Uuid,
    identity_id: Uuid,
) -> Result<Option<IdentityRemoved>, AuthError> {
    let row = sqlx::query(
        "SELECT i.removed_at, p.account_link_generation, \
         (SELECT count(*) FROM relay_credentials c WHERE c.identity_id = i.identity_id AND c.revoked_at IS NOT NULL) AS credentials, \
         (SELECT count(*) FROM browser_sessions s WHERE s.identity_id = i.identity_id AND s.revoked_at IS NOT NULL) AS sessions \
         FROM oidc_identities i JOIN principals p ON p.id = i.principal_id \
         WHERE i.identity_id = $1 AND i.principal_id = $2 AND i.removed_at IS NOT NULL FOR SHARE",
    )
    .bind(identity_id)
    .bind(principal_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(retryable_database_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(IdentityRemoved {
        identity_id,
        principal_id: PrincipalId::from_uuid(principal_id),
        removed_at: row.get("removed_at"),
        account_link_generation: row.get("account_link_generation"),
        revoked_credentials: u32::try_from(row.get::<i64, _>("credentials"))
            .map_err(|_error| AuthError::Durable)?,
        revoked_sessions: u32::try_from(row.get::<i64, _>("sessions"))
            .map_err(|_error| AuthError::Durable)?,
    }))
}

/// Truncates an over-fetched page and returns its continuation cursor.
fn link_page_cursor(
    records: &mut Vec<AccountLinkRecord>,
    limit: i64,
) -> Result<Option<Uuid>, AuthError> {
    let limit = usize::try_from(limit).map_err(|_error| AuthError::Durable)?;
    if records.len() <= limit {
        return Ok(None);
    }
    records.truncate(limit);
    Ok(records.last().map(|record| record.link_id))
}

/// Projects one durable link row onto its safe public record.
fn link_record(row: &sqlx::postgres::PgRow) -> Result<AccountLinkRecord, AuthError> {
    let channel = match row.get::<String, _>("channel").as_str() {
        "browser" => AccountLinkChannel::Browser,
        "device" => AccountLinkChannel::Device,
        _ => return Err(AuthError::Durable),
    };
    let state = match row.get::<String, _>("state").as_str() {
        "pending" => AccountLinkState::Pending,
        "completed" => AccountLinkState::Completed,
        "cancelled" => AccountLinkState::Cancelled,
        "expired" => AccountLinkState::Expired,
        "failed" => AccountLinkState::Failed,
        _ => return Err(AuthError::Durable),
    };
    Ok(AccountLinkRecord {
        link_id: row.get("link_id"),
        principal_id: PrincipalId::from_uuid(row.get("principal_id")),
        channel,
        state,
        source_identity_id: row.get("source_identity_id"),
        linked_identity_id: row.get("linked_identity_id"),
        revision: row.get("revision"),
        account_link_generation: row.get("account_link_generation"),
        created_at: row.get("created_at"),
        expires_at: row.get("expires_at"),
        completed_at: row.get("completed_at"),
    })
}

/// Classifies a link-transaction write without exposing database detail.
fn link_database_error(error: sqlx::Error) -> AuthError {
    if is_unique_violation(&error, "account_link_transactions_single_pending_idx") {
        return AuthError::LinkPending;
    }
    if is_unique_violation(&error, "account_link_transactions_idempotency_key") {
        return AuthError::IdempotencyConflict;
    }
    retryable_database_error(error)
}

/// Classifies the identity insert that completes a link.
fn link_identity_error(error: sqlx::Error) -> AuthError {
    if is_unique_violation(&error, "oidc_identities_active_coordinate_idx") {
        return AuthError::LinkCollision;
    }
    if is_unique_violation(&error, "oidc_identities_link_once_idx") {
        return AuthError::LinkReplayed;
    }
    retryable_database_error(error)
}

/// Reports whether one failure is this exact unique-constraint violation.
fn is_unique_violation(error: &sqlx::Error, constraint: &str) -> bool {
    error.as_database_error().is_some_and(|database| {
        database.code().is_some_and(|code| code == UNIQUE_VIOLATION)
            && database.constraint() == Some(constraint)
    })
}
