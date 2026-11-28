//! Verified resolution of the local daemon socket endpoint.
//!
//! The resolver is the shared path contract of `pohunek-paths`
//! ([`pohunek_paths::resolve_runtime_dir`], [`pohunek_paths::validate_socket_path`])
//! plus the private-endpoint check of [`crate::filesystem::verify_private_socket_endpoint`],
//! which runs on the `TrustedDir` validation of [`crate::filesystem`] — one
//! implementation, including the ACL and ancestor checks (#2563). The error
//! contract stays in `pohunek-paths` ([`pohunek_paths::PathError`]), so callers
//! map one typed error regardless of where the check runs.

// Rust guideline compliant 2026-11-28

use std::path::PathBuf;

use pohunek_paths::{PathEnv, PathError, Platform, SocketKind, SOCKET_NAME};

/// Resolves the local control socket path and verifies that an existing
/// runtime directory and socket are private to the caller.
///
/// # Errors
///
/// Returns the [`PathError`] of the path contract, or
/// [`PathError::UnsafeSocketEndpoint`] when an existing entry or one of its
/// ancestors is not private.
pub fn verified_socket_path() -> Result<PathBuf, PathError> {
    let platform = Platform::current()?;
    let effective_uid = rustix::process::geteuid().as_raw();
    verified_socket_path_for(platform, effective_uid, &PathEnv::capture())
}

/// [`verified_socket_path`] from explicit platform, identity and environment
/// inputs, so every platform and failure can be exercised without changing the
/// process environment.
///
/// # Errors
///
/// Same as [`verified_socket_path`].
pub fn verified_socket_path_for(
    platform: Platform,
    effective_uid: u32,
    env: &PathEnv,
) -> Result<PathBuf, PathError> {
    let runtime_dir = pohunek_paths::resolve_runtime_dir(platform, effective_uid, env)?;
    let socket = runtime_dir.join(SOCKET_NAME);
    pohunek_paths::validate_socket_path(&socket, platform, SocketKind::Daemon)?;
    crate::filesystem::verify_private_socket_endpoint(&runtime_dir, &socket, effective_uid)?;
    Ok(socket)
}
