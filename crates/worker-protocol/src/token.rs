//! One-use data-stream token state.
//!
//! The vault contains no entropy source. Workers generate random
//! [`DataToken`] values with their established cryptographic entropy provider,
//! then register time and identity claims here. Callers supply monotonic
//! millisecond timestamps, keeping this state machine deterministic.

// Rust guideline compliant 2026-06-26

use std::collections::HashMap;

use thiserror::Error;

use crate::{DataToken, LeaseId, RuntimeId, StreamId};

/// Binds a one-use data token to one lease and runtime stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenClaims {
    /// Controller lease that minted the token.
    pub lease_id: LeaseId,
    /// Runtime generation authorized by the token.
    pub runtime_id: RuntimeId,
    /// Exact data stream authorized by the token.
    pub stream_id: StreamId,
    /// Worker monotonic millisecond expiry.
    pub expires_at_ms: u64,
}

/// Stores bounded-lifetime one-use data tokens.
#[derive(Debug)]
pub struct TokenVault {
    entries: HashMap<DataToken, TokenClaims>,
    maximum: usize,
}

impl TokenVault {
    /// Creates an empty bounded token vault.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError::InvalidCapacity`] when `maximum` is zero.
    pub fn new(maximum: usize) -> Result<Self, TokenError> {
        if maximum == 0 {
            return Err(TokenError::InvalidCapacity);
        }
        Ok(Self {
            entries: HashMap::with_capacity(maximum),
            maximum,
        })
    }

    /// Registers a newly generated token.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError::Duplicate`] when the token already exists, or
    /// [`TokenError::AlreadyExpired`] when its expiry is not after `now_ms`.
    pub fn insert(
        &mut self,
        token: DataToken,
        claims: TokenClaims,
        now_ms: u64,
    ) -> Result<(), TokenError> {
        if claims.expires_at_ms <= now_ms {
            return Err(TokenError::AlreadyExpired);
        }
        let _purged = self.purge_expired(now_ms);
        if self.entries.contains_key(&token) {
            return Err(TokenError::Duplicate);
        }
        if self.entries.len() >= self.maximum {
            return Err(TokenError::Full {
                maximum: self.maximum,
            });
        }
        self.entries.insert(token, claims);
        Ok(())
    }

    /// Redeems and consumes a token for an exact identity scope.
    ///
    /// A known token is consumed even when expired or mismatched. This prevents
    /// repeated probing and guarantees every successful credential is one-use.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError`] when the token is unknown, expired, already used,
    /// or bound to different lease, runtime, or stream identities.
    pub fn redeem(
        &mut self,
        token: &DataToken,
        lease_id: &LeaseId,
        runtime_id: &RuntimeId,
        stream_id: &StreamId,
        now_ms: u64,
    ) -> Result<TokenClaims, TokenError> {
        let claims = self.entries.remove(token).ok_or(TokenError::Unknown)?;
        if claims.expires_at_ms <= now_ms {
            return Err(TokenError::Expired);
        }
        if &claims.lease_id != lease_id
            || &claims.runtime_id != runtime_id
            || &claims.stream_id != stream_id
        {
            return Err(TokenError::ScopeMismatch);
        }
        Ok(claims)
    }

    /// Removes tokens expired at or before `now_ms`.
    ///
    /// Returns the number of removed entries.
    #[must_use]
    pub fn purge_expired(&mut self, now_ms: u64) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|_, claims| claims.expires_at_ms > now_ms);
        before - self.entries.len()
    }

    /// Returns the number of currently retained credentials.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Reports whether the vault contains no credentials.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Reports one-use data-token state failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TokenError {
    /// A vault was configured without capacity for any token.
    #[error("data token vault capacity must be nonzero")]
    InvalidCapacity,
    /// The bounded vault cannot accept another live token.
    #[error("data token vault is full at its configured maximum of {maximum}")]
    Full {
        /// Configured live-token maximum.
        maximum: usize,
    },
    /// A generated token collided with an existing entry.
    #[error("data token is already registered")]
    Duplicate,
    /// An insertion attempted to register an already expired token.
    #[error("data token expiry must be after the current time")]
    AlreadyExpired,
    /// The token was absent or had already been redeemed.
    #[error("data token is unknown or already redeemed")]
    Unknown,
    /// The token expired before redemption.
    #[error("data token has expired")]
    Expired,
    /// Lease, runtime, or stream identity did not match its claims.
    #[error("data token scope does not match the requested stream")]
    ScopeMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(expires_at_ms: u64) -> TokenClaims {
        TokenClaims {
            lease_id: LeaseId::new("lease-1").expect("valid lease"),
            runtime_id: RuntimeId::new("runtime-1").expect("valid runtime"),
            stream_id: StreamId::new("stream-1").expect("valid stream"),
            expires_at_ms,
        }
    }

    #[test]
    fn successful_token_is_redeemable_exactly_once() {
        let token = DataToken::new("token-1").expect("valid token");
        let expected = claims(20);
        let mut vault = TokenVault::new(4).expect("valid vault capacity");
        vault
            .insert(token.clone(), expected.clone(), 10)
            .expect("insert live token");

        assert_eq!(
            vault
                .redeem(
                    &token,
                    &expected.lease_id,
                    &expected.runtime_id,
                    &expected.stream_id,
                    15,
                )
                .expect("redeem matching live token"),
            expected
        );
        assert_eq!(
            vault
                .redeem(
                    &token,
                    &LeaseId::new("lease-1").expect("valid lease"),
                    &RuntimeId::new("runtime-1").expect("valid runtime"),
                    &StreamId::new("stream-1").expect("valid stream"),
                    15,
                )
                .expect_err("second redemption must fail"),
            TokenError::Unknown
        );
    }

    #[test]
    fn expired_and_mismatched_tokens_are_consumed() {
        let expired = DataToken::new("expired").expect("valid token");
        let mismatched = DataToken::new("mismatched").expect("valid token");
        let expected = claims(20);
        let mut vault = TokenVault::new(4).expect("valid vault capacity");
        vault
            .insert(expired.clone(), expected.clone(), 10)
            .expect("insert token");
        vault
            .insert(mismatched.clone(), expected.clone(), 10)
            .expect("insert token");

        assert_eq!(
            vault
                .redeem(
                    &expired,
                    &expected.lease_id,
                    &expected.runtime_id,
                    &expected.stream_id,
                    20,
                )
                .expect_err("expired token must fail"),
            TokenError::Expired
        );
        assert_eq!(
            vault
                .redeem(
                    &mismatched,
                    &LeaseId::new("other").expect("valid lease"),
                    &expected.runtime_id,
                    &expected.stream_id,
                    15,
                )
                .expect_err("mismatched token must fail"),
            TokenError::ScopeMismatch
        );
        assert!(vault.is_empty());
    }

    #[test]
    fn bounded_vault_purges_expired_entries_before_rejecting_insert() {
        let first = DataToken::new("first").expect("valid token");
        let second = DataToken::new("second").expect("valid token");
        let mut vault = TokenVault::new(1).expect("valid vault capacity");
        vault
            .insert(first, claims(20), 10)
            .expect("insert first token");
        vault
            .insert(second, claims(30), 20)
            .expect("expired first token is purged");

        assert_eq!(vault.len(), 1);
    }
}
