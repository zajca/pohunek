//! Assistant launch-id and hash plumbing.

use sha2::{Digest, Sha256};

/// Pohunek version of this build; keys launch ids and the launch snapshot.
pub const POHUNEK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Return a unique runtime launch id for one assistant materialization.
///
/// # Panics
///
/// Panics if the system clock is set before the Unix epoch.
#[must_use]
pub fn assistant_launch_id(version_hash: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is after unix epoch")
        .as_nanos();
    format!(
        "launch-{version_hash}-{}-{nanos}-{sequence}",
        std::process::id()
    )
}

/// Return the runtime launch id of the assistant launch that carries
/// `request_id`.
///
/// The same request id gives the same id, so a launch repeated under it renders
/// the same snapshot path into its opening prompt and the daemon sees the same
/// request.
#[must_use]
pub fn assistant_keyed_launch_id(version_hash: &str, request_id: &str) -> String {
    let digest = Sha256::digest(request_id.as_bytes());
    format!("launch-{version_hash}-request-{digest:x}")
}

/// Compute a SHA-256 digest for a byte slice.
#[must_use]
pub fn sha256_for_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}
