//! Bounds process-local OAuth transaction secrets.

// Rust guideline compliant 2026-09-09

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
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingDeviceCode")
            .field("redacted", &true)
            .finish()
    }
}

/// Stores process-local device codes after shared admission has succeeded.
#[derive(Debug, Clone, Default)]
pub(crate) struct PendingDeviceCodes {
    codes: Arc<Mutex<HashMap<Uuid, PendingDeviceCode>>>,
}

impl PendingDeviceCodes {
    /// Inserts one admitted device code.
    pub(crate) fn insert(&self, login_id: Uuid, code: PendingDeviceCode) -> Result<(), AuthError> {
        let mut codes = self.codes.lock().map_err(|_error| AuthError::Durable)?;
        if codes.insert(login_id, code).is_some() {
            return Err(AuthError::Malformed);
        }
        Ok(())
    }

    /// Removes a device code for its one terminal exchange.
    pub(crate) fn take(&self, login_id: Uuid) -> Option<PendingDeviceCode> {
        let mut codes = self.codes.lock().ok()?;
        let code = codes.remove(&login_id)?;
        (!code.expired(Instant::now())).then_some(code)
    }

    /// Returns a code to the map after a nonterminal token response.
    pub(crate) fn restore(&self, login_id: Uuid, code: PendingDeviceCode) {
        if code.expired(Instant::now()) {
            return;
        }
        let Ok(mut codes) = self.codes.lock() else {
            return;
        };
        let replaced = codes.insert(login_id, code);
        debug_assert!(
            replaced.is_none(),
            "a claimed pending device code must not be replaced"
        );
    }

    /// Discards codes whose matching durable transaction can no longer issue a credential.
    pub(crate) fn prune_expired(&self) {
        if let Ok(mut codes) = self.codes.lock() {
            codes.retain(|_, code| !code.expired(Instant::now()));
        }
    }

    /// Removes every pending code during shutdown or recovery quarantine.
    pub(crate) fn clear(&self) {
        if let Ok(mut codes) = self.codes.lock() {
            codes.clear();
        }
    }
}

/// Enforces one capacity bound across every pending OAuth transaction.
#[derive(Debug, Clone)]
pub(crate) struct PendingTransactions {
    capacity: usize,
    state: Arc<Mutex<PendingTransactionState>>,
}

#[derive(Debug, Default)]
struct PendingTransactionState {
    transactions: HashMap<Uuid, Instant>,
    reservations: usize,
}

/// Holds one capacity unit before a transaction performs durable or issuer work.
#[derive(Debug)]
pub(crate) struct PendingTransactionReservation {
    state: Arc<Mutex<PendingTransactionState>>,
    active: bool,
}

impl PendingTransactions {
    /// Creates a shared, explicitly bounded pending-transaction ledger.
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Arc::new(Mutex::new(PendingTransactionState::default())),
        }
    }

    /// Reserves capacity before a flow can make durable or issuer work.
    pub(crate) fn reserve(&self) -> Result<PendingTransactionReservation, AuthError> {
        let mut state = self.state.lock().map_err(|_error| AuthError::Durable)?;
        state
            .transactions
            .retain(|_, expires_at| *expires_at > Instant::now());
        if state.transactions.len().saturating_add(state.reservations) >= self.capacity {
            return Err(AuthError::Capacity);
        }
        state.reservations += 1;
        Ok(PendingTransactionReservation {
            state: Arc::clone(&self.state),
            active: true,
        })
    }

    /// Releases a terminal transaction's capacity unit.
    pub(crate) fn release(&self, login_id: Uuid) {
        if let Ok(mut state) = self.state.lock() {
            state.transactions.remove(&login_id);
        }
    }

    /// Clears every transaction during shutdown or recovery quarantine.
    pub(crate) fn clear(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.transactions.clear();
        }
    }
}

impl PendingTransactionReservation {
    /// Transfers one reservation to an active login until the supplied expiry.
    pub(crate) fn commit(mut self, login_id: Uuid, expires_at: Instant) -> Result<(), AuthError> {
        let mut state = self.state.lock().map_err(|_error| AuthError::Durable)?;
        if !self.active
            || state.reservations == 0
            || state.transactions.insert(login_id, expires_at).is_some()
        {
            return Err(AuthError::Malformed);
        }
        state.reservations -= 1;
        self.active = false;
        Ok(())
    }
}

impl Drop for PendingTransactionReservation {
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
    use std::time::{Duration, Instant};

    use super::{AuthError, PendingTransactions};
    use uuid::Uuid;

    #[test]
    fn shared_bound_rejects_a_second_flow_before_it_can_start() {
        let pending = PendingTransactions::new(1);
        let browser = pending.reserve().expect("reserve browser login capacity");
        assert!(matches!(pending.reserve(), Err(AuthError::Capacity)));
        browser
            .commit(Uuid::now_v7(), Instant::now() + Duration::from_mins(1))
            .expect("browser login owns the shared capacity unit");
        assert!(matches!(pending.reserve(), Err(AuthError::Capacity)));
    }

    #[test]
    fn cancelled_and_expired_flows_admit_the_next_flow() {
        let pending = PendingTransactions::new(1);
        let cancelled = pending.reserve().expect("reserve cancelled flow");
        drop(cancelled);
        let expired = Uuid::now_v7();
        pending
            .reserve()
            .expect("cancelled reservation releases capacity")
            .commit(expired, Instant::now())
            .expect("commit already expired test flow");
        pending
            .reserve()
            .expect("expiry cleanup occurs before the next admission");
    }

    #[test]
    fn terminal_release_admits_another_flow() {
        let pending = PendingTransactions::new(1);
        let login_id = Uuid::now_v7();
        pending
            .reserve()
            .expect("reserve active flow")
            .commit(login_id, Instant::now() + Duration::from_mins(1))
            .expect("commit active flow");
        pending.release(login_id);
        pending
            .reserve()
            .expect("terminal flow releases capacity for the next admission");
    }
}
