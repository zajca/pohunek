//! Native relay defaults live separately from command behavior.

use std::time::Duration;

/// Native auth/account operations should finish promptly even when a relay stalls.
pub(super) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Relay credential/account replies contain metadata, never session contents.
pub(super) const RESPONSE_BYTES: usize = 16 * 1024;
/// Namespaces relay credentials separately from provider tokens and daemon state.
pub(super) const KEYRING_SERVICE: &str = "pohunek-relay";
/// Domain separation prevents a provider key from colliding with a relay origin.
pub(super) const KEYRING_DOMAIN: &[u8] = b"pohunek-relay-origin-v1\0";
/// Bounds decoding of the single private keyring record.
pub(super) const KEYRING_BYTES: usize = 16 * 1024;
/// Rejects unrelated or stale keyring record layouts before authentication.
pub(super) const KEYRING_VERSION: u16 = 1;
/// Custom public CA bundles must be small and are never private signing keys.
pub(super) const CA_BYTES: u64 = 64 * 1024;
/// Serializes mutations of each origin's single keyring credential.
pub(super) const LOCK_DIRECTORY: &str = "relay-locks";
/// Only the local operator may replace or lock a credential transaction.
pub(super) const DIRECTORY_MODE: u32 = 0o700;
pub(super) const FILE_MODE: u32 = 0o600;
/// Matches the relay's maximum configured overlap ceiling.
pub(super) const MAX_OVERLAP_SECONDS: u32 = 300;
/// Covers two bounded POST attempts, account validation, and compensation.
pub(super) const MIN_OVERLAP_SECONDS: u32 = 60;
/// Native mutations reserve the 60-second overlap plus one 10-second POST.
pub(super) const MIN_REMAINING: time::Duration = time::Duration::seconds(70);
