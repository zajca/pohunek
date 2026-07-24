//! Enforces one connection-bound daemon controller.

// Rust guideline compliant 2026-07-23

use std::sync::{Arc, Mutex};

/// Stable identity of one daemon control connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseOwner {
    /// Daemon instance identifier.
    pub daemon_id: String,
    /// Peer process identifier from Unix credentials.
    pub peer_pid: u32,
    /// Peer process start identity.
    pub peer_start_identity: String,
}

/// Controller lease failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LeaseError {
    /// Another live controller owns the worker.
    #[error("worker controller is already leased")]
    Busy,
    /// Lease ID or owner does not match the active controller.
    #[error("worker controller lease does not match")]
    Mismatch,
}

/// Cloneable single-controller lease book.
#[derive(Debug, Clone)]
pub struct ControllerLease {
    active: Arc<Mutex<Option<LeaseRecord>>>,
}

impl ControllerLease {
    /// Creates an unleased controller slot.
    #[must_use]
    pub fn new() -> Self {
        Self {
            active: Arc::new(Mutex::new(None)),
        }
    }

    /// Acquires the worker for `owner`.
    ///
    /// Repeating the exact owner and lease is idempotent. No time-based lease
    /// stealing exists.
    ///
    /// # Errors
    ///
    /// Returns [`LeaseError::Busy`] when another owner is connected.
    pub fn acquire(&self, owner: LeaseOwner, lease_id: String) -> Result<(), LeaseError> {
        let mut active = lock(&self.active);
        match active.as_ref() {
            Some(record) if record.owner == owner && record.lease_id == lease_id => Ok(()),
            Some(_) => Err(LeaseError::Busy),
            None => {
                *active = Some(LeaseRecord { owner, lease_id });
                Ok(())
            }
        }
    }

    /// Validates a mutating request against the active lease.
    ///
    /// # Errors
    ///
    /// Returns [`LeaseError::Mismatch`] when identity or lease differs.
    pub fn validate(&self, owner: &LeaseOwner, lease_id: &str) -> Result<(), LeaseError> {
        let active = lock(&self.active);
        if active
            .as_ref()
            .is_some_and(|record| record.owner == *owner && record.lease_id == lease_id)
        {
            Ok(())
        } else {
            Err(LeaseError::Mismatch)
        }
    }

    /// Releases the exact active controller.
    ///
    /// # Errors
    ///
    /// Returns [`LeaseError::Mismatch`] for a stale connection.
    pub fn release(&self, owner: &LeaseOwner, lease_id: &str) -> Result<(), LeaseError> {
        self.validate(owner, lease_id)?;
        *lock(&self.active) = None;
        Ok(())
    }

    /// Releases a connection at EOF without trusting a request payload.
    pub fn release_connection(&self, owner: &LeaseOwner) {
        let mut active = lock(&self.active);
        if active.as_ref().is_some_and(|record| record.owner == *owner) {
            *active = None;
        }
    }

    /// Returns whether any controller currently owns the worker.
    #[must_use]
    pub fn is_active(&self) -> bool {
        lock(&self.active).is_some()
    }
}

impl Default for ControllerLease {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LeaseRecord {
    owner: LeaseOwner,
    lease_id: String,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::{ControllerLease, LeaseError, LeaseOwner};

    fn owner(id: &str, pid: u32) -> LeaseOwner {
        LeaseOwner {
            daemon_id: id.to_owned(),
            peer_pid: pid,
            peer_start_identity: format!("start-{pid}"),
        }
    }

    #[test]
    fn one_controller_is_exclusive_without_stealing() {
        let leases = ControllerLease::new();
        leases
            .acquire(owner("daemon-a", 10), "lease-a".to_owned())
            .expect("acquire first");

        assert_eq!(
            leases.acquire(owner("daemon-b", 11), "lease-b".to_owned()),
            Err(LeaseError::Busy)
        );
        assert!(leases.is_active());
    }

    #[test]
    fn eof_release_allows_replacement_controller() {
        let leases = ControllerLease::new();
        let first = owner("daemon-a", 10);
        leases
            .acquire(first.clone(), "lease-a".to_owned())
            .expect("acquire");
        leases.release_connection(&first);

        leases
            .acquire(owner("daemon-b", 11), "lease-b".to_owned())
            .expect("replacement");
    }

    #[test]
    fn stale_release_cannot_clear_current_controller() {
        let leases = ControllerLease::new();
        let current = owner("daemon-a", 10);
        leases
            .acquire(current.clone(), "lease-a".to_owned())
            .expect("acquire");

        assert_eq!(
            leases.release(&owner("daemon-a", 99), "lease-a"),
            Err(LeaseError::Mismatch)
        );
        leases.validate(&current, "lease-a").expect("still active");
    }
}
