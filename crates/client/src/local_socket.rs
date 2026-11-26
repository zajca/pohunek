//! Validated resolution of the local daemon socket path.

use std::path::PathBuf;

use pohunek_paths::{PathEnv, PathError, Platform};

use crate::ClientError;

/// Resolves the local daemon socket path from the process environment.
///
/// The path is the one the daemon binds. An existing runtime directory or
/// socket must be private to the caller: not a symlink, owned by the effective
/// user, and without group or other access. A missing entry passes, because
/// connecting then reports that no daemon is listening.
///
/// # Errors
///
/// Returns [`ClientError::LocalSocketUnresolvable`] when the runtime directory
/// cannot be derived, [`ClientError::LocalSocketPathInvalid`] when the socket
/// path violates the path contract, and [`ClientError::LocalSocketUnsafe`]
/// when an existing entry is not private.
pub fn local_socket_path() -> Result<PathBuf, ClientError> {
    pohunek_paths::verified_socket_path().map_err(classify)
}

/// [`local_socket_path`] from explicit platform, identity and environment
/// inputs.
///
/// # Errors
///
/// Same as [`local_socket_path`].
pub fn local_socket_path_for(
    platform: Platform,
    effective_uid: u32,
    env: &PathEnv,
) -> Result<PathBuf, ClientError> {
    pohunek_paths::verified_socket_path_for(platform, effective_uid, env).map_err(classify)
}

/// Maps a resolver failure to the client error of its kind.
fn classify(source: PathError) -> ClientError {
    match source {
        PathError::SocketPathContainsNul { .. }
        | PathError::SocketPathNotAbsolute { .. }
        | PathError::SocketPathParentComponent { .. }
        | PathError::SocketPathTooLong { .. } => ClientError::LocalSocketPathInvalid { source },
        PathError::UnsafeSocketEndpoint { .. } => ClientError::LocalSocketUnsafe { source },
        PathError::MissingEnv { .. }
        | PathError::InvalidEnv { .. }
        | PathError::InvalidInstallPrefix { .. }
        | PathError::UnsupportedPlatform { .. } => ClientError::LocalSocketUnresolvable { source },
    }
}
