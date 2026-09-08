//! Bounds process-local device authorization secrets.

// Rust guideline compliant 2026-09-08

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};
use uuid::Uuid;

use super::{oidc::OidcDeviceAuthorization, AuthError};

/// Keeps one unpersisted OAuth device code.
pub(crate) struct PendingDeviceCode {
    authorization: OidcDeviceAuthorization,
    expires_at: Instant,
}

impl PendingDeviceCode {
    /// Creates a pending device code from the issuer response.
    pub(crate) fn new(authorization: OidcDeviceAuthorization) -> Self {
        let expires_at = Instant::now() + authorization.expires_in;
        Self {
            authorization,
            expires_at,
        }
    }

    /// Borrows the code only for the issuer token exchange.
    pub(crate) fn authorization(&self) -> &OidcDeviceAuthorization {
        &self.authorization
    }

    fn expired(&self, now: Instant) -> bool {
        self.expires_at <= now
    }
}

impl std::fmt::Debug for PendingDeviceCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingDeviceCode")
            .field("redacted", &true)
            .finish()
    }
}

/// Bounds device codes that must disappear after process restart.
#[derive(Debug, Clone)]
pub(crate) struct PendingDeviceCodes {
    capacity: usize,
    state: Arc<Mutex<PendingDeviceState>>,
}

#[derive(Debug, Default)]
struct PendingDeviceState {
    codes: HashMap<Uuid, PendingDeviceCode>,
    reservations: usize,
}

/// Holds one capacity unit from before an upstream device authorization starts.
#[derive(Debug)]
pub(crate) struct PendingDeviceReservation {
    state: Arc<Mutex<PendingDeviceState>>,
    active: bool,
}

impl PendingDeviceCodes {
    /// Creates a bounded pending-device store.
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Arc::new(Mutex::new(PendingDeviceState::default())),
        }
    }

    /// Reserves capacity before an issuer call so a full relay performs no work upstream.
    pub(crate) fn reserve(&self) -> Result<PendingDeviceReservation, AuthError> {
        let mut state = self.state.lock().map_err(|_| AuthError::Durable)?;
        state
            .codes
            .retain(|_, pending| !pending.expired(Instant::now()));
        if state.codes.len().saturating_add(state.reservations) >= self.capacity {
            return Err(AuthError::Capacity);
        }
        state.reservations += 1;
        Ok(PendingDeviceReservation {
            state: Arc::clone(&self.state),
            active: true,
        })
    }

    /// Removes a device code for its one terminal exchange.
    pub(crate) async fn take(&self, login_id: Uuid) -> Option<PendingDeviceCode> {
        self.state.lock().ok()?.codes.remove(&login_id)
    }

    /// Returns a code to the map after a nonterminal token response.
    pub(crate) async fn restore(&self, login_id: Uuid, code: PendingDeviceCode) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let replaced = state.codes.insert(login_id, code);
        debug_assert!(
            replaced.is_none(),
            "a claimed pending device code must not be replaced"
        );
    }

    /// Removes every pending code during shutdown or recovery quarantine.
    pub(crate) async fn clear(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.codes.clear();
        }
    }
}

impl PendingDeviceReservation {
    /// Transfers the reserved capacity to exactly one durable login ID.
    pub(crate) fn commit(
        mut self,
        login_id: Uuid,
        code: PendingDeviceCode,
    ) -> Result<(), AuthError> {
        let mut state = self.state.lock().map_err(|_| AuthError::Durable)?;
        if !self.active || state.reservations == 0 || state.codes.insert(login_id, code).is_some() {
            return Err(AuthError::Malformed);
        }
        state.reservations -= 1;
        self.active = false;
        Ok(())
    }
}

impl Drop for PendingDeviceReservation {
    fn drop(&mut self) {
        if self.active {
            if let Ok(mut state) = self.state.lock() {
                state.reservations = state.reservations.saturating_sub(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{AuthError, OidcDeviceAuthorization, PendingDeviceCode, PendingDeviceCodes};
    use uuid::Uuid;
    use zeroize::Zeroizing;

    fn authorization(expires_in: u64) -> OidcDeviceAuthorization {
        OidcDeviceAuthorization {
            verification_uri: url::Url::parse("https://issuer.example/device")
                .expect("valid fixture URL"),
            verification_uri_complete: None,
            user_code: "user-code".to_owned(),
            expires_in: Duration::from_secs(expires_in),
            interval: Duration::from_secs(1),
            device_code: Zeroizing::new("device-code".to_owned()),
            verifier: Zeroizing::new("a".repeat(43)),
        }
    }

    #[tokio::test]
    async fn expired_device_code_frees_capacity_before_admission() {
        let pending = PendingDeviceCodes::new(1);
        pending
            .reserve()
            .expect("reserve capacity")
            .commit(Uuid::now_v7(), PendingDeviceCode::new(authorization(0)))
            .expect("expired entry initially occupies the map");

        pending
            .reserve()
            .expect("expired entry frees capacity")
            .commit(Uuid::now_v7(), PendingDeviceCode::new(authorization(60)))
            .expect("capacity is reclaimed before admission");
    }

    #[tokio::test]
    async fn current_device_code_keeps_capacity_bound() {
        let pending = PendingDeviceCodes::new(1);
        pending
            .reserve()
            .expect("reserve current entry")
            .commit(Uuid::now_v7(), PendingDeviceCode::new(authorization(60)))
            .expect("first current entry is accepted");

        let error = pending
            .reserve()
            .expect_err("second current entry exceeds the configured bound");
        assert!(matches!(error, AuthError::Capacity));
    }

    #[test]
    fn reservation_blocks_concurrent_upstream_work_and_releases_on_drop() {
        let pending = PendingDeviceCodes::new(1);
        let reservation = pending.reserve().expect("reserve the sole capacity unit");
        assert!(matches!(pending.reserve(), Err(AuthError::Capacity)));
        drop(reservation);
        pending
            .reserve()
            .expect("dropped reservation returns capacity before another upstream call");
    }

    #[test]
    fn reservation_transfers_capacity_to_one_code() {
        let pending = PendingDeviceCodes::new(1);
        pending
            .reserve()
            .expect("reserve capacity")
            .commit(Uuid::nil(), PendingDeviceCode::new(authorization(60)))
            .expect("bind reservation exactly once");
        assert!(matches!(pending.reserve(), Err(AuthError::Capacity)));
    }
}
