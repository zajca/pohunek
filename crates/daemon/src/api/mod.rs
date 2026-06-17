//! Unix-socket control server.
//!
//! Binds the control socket with owner-private permissions, recovers from a
//! stale socket left by a previous run, and serves newline-delimited JSON
//! requests using the shared `protocol` crate. Each connection is handled on its
//! own Tokio task so one client cannot stall another, and a panicking handler
//! cannot take down the daemon (per `docs/architecture.md` "Concurrency and
//! supervision").
//!
//! Milestone 2 implements `daemon.health` only; unknown methods receive a typed
//! `method_not_found` error (the contract for later milestones is already in the
//! `protocol` crate).
//!
//! Attach streaming uses a *separate* connection and is a later milestone; this
//! server handles the JSON control connection only.

mod handler;

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use futures::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::codec::{Framed, LinesCodec, LinesCodecError};
use tracing::{error, info, warn};

use crate::error::DaemonError;

pub use handler::{handle_request, HealthInfo};

/// Directory mode for the runtime dir: owner rwx only (`0700`).
const DIR_MODE: u32 = 0o700;
/// Socket mode: owner rw only (`0600`).
const SOCKET_MODE: u32 = 0o600;
/// Max accepted control line length, in bytes. Bounds memory per connection;
/// control envelopes are small. Raw terminal bytes never travel here.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// The bound control server, ready to accept connections.
#[derive(Debug)]
pub struct ControlServer {
    listener: UnixListener,
    socket_path: PathBuf,
    health: HealthInfo,
}

impl ControlServer {
    /// Bind the control socket at `socket_path`.
    ///
    /// The parent directory is created (mode `0700`) if missing. If a socket
    /// file already exists, it is probed: a live daemon there is a hard error
    /// (the single-instance lock should have caught this first), while a stale
    /// socket (nothing listening) is removed and rebound (stale-socket recovery,
    /// per the plan's milestone 2).
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError`] on directory, permission, probe, or bind failure.
    pub async fn bind(socket_path: &Path, health: HealthInfo) -> Result<Self, DaemonError> {
        let dir = socket_path
            .parent()
            .ok_or_else(|| DaemonError::Socket {
                path: socket_path.to_path_buf(),
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "socket path has no parent directory",
                ),
            })?;

        ensure_dir_mode(dir, DIR_MODE)?;
        recover_stale_socket(socket_path).await?;

        let listener = UnixListener::bind(socket_path).map_err(|source| DaemonError::Socket {
            path: socket_path.to_path_buf(),
            source,
        })?;

        set_mode(socket_path, SOCKET_MODE)?;

        info!(socket = %socket_path.display(), "control socket bound");
        Ok(Self {
            listener,
            socket_path: socket_path.to_path_buf(),
            health,
        })
    }

    /// The bound socket path.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Run the accept loop until `shutdown` resolves.
    ///
    /// Each accepted connection is served on its own task. The loop itself never
    /// returns an error for a single bad connection; transient accept errors are
    /// logged and the loop continues.
    pub async fn serve(self, shutdown: impl std::future::Future<Output = ()>) {
        tokio::pin!(shutdown);
        let health = self.health.clone();
        loop {
            tokio::select! {
                () = &mut shutdown => {
                    info!("shutdown signal received; stopping accept loop");
                    break;
                }
                accepted = self.listener.accept() => {
                    match accepted {
                        Ok((stream, _addr)) => {
                            let health = health.clone();
                            tokio::spawn(async move {
                                if let Err(err) = serve_connection(stream, health).await {
                                    warn!(error = %err, "control connection ended with error");
                                }
                            });
                        }
                        Err(err) => {
                            // A failed accept is transient (e.g. fd limit); log
                            // and keep serving rather than crashing the daemon.
                            error!(error = %err, "accept failed");
                        }
                    }
                }
            }
        }
        // Best-effort cleanup so the next start does not need stale-socket
        // recovery. A failure here is non-fatal.
        if let Err(err) = std::fs::remove_file(&self.socket_path) {
            if err.kind() != io::ErrorKind::NotFound {
                warn!(error = %err, socket = %self.socket_path.display(), "failed to remove socket on shutdown");
            }
        }
    }
}

/// Serve one control connection: read newline-delimited JSON requests, dispatch
/// each, and write back one response line per request.
async fn serve_connection(stream: UnixStream, health: HealthInfo) -> Result<(), io::Error> {
    let codec = LinesCodec::new_with_max_length(MAX_LINE_BYTES);
    let mut framed = Framed::new(stream, codec);

    while let Some(line) = framed.next().await {
        let line = match line {
            Ok(line) => line,
            Err(LinesCodecError::MaxLineLengthExceeded) => {
                warn!("control line exceeded max length; closing connection");
                break;
            }
            Err(LinesCodecError::Io(err)) => return Err(err),
        };

        let response_line = handler::dispatch_line(&line, &health);
        framed
            .send(response_line)
            .await
            .map_err(|e| match e {
                LinesCodecError::Io(io) => io,
                LinesCodecError::MaxLineLengthExceeded => {
                    io::Error::new(io::ErrorKind::InvalidData, "response exceeded max line length")
                }
            })?;
    }
    Ok(())
}

/// Probe an existing socket file and recover if it is stale.
///
/// If `path` does not exist, do nothing. If it exists and is connectable, a live
/// daemon is there (the single-instance lock should normally prevent reaching
/// here): return an error. If it exists but refuses connection, treat it as a
/// stale socket from a previous run and remove it.
async fn recover_stale_socket(path: &Path) -> Result<(), DaemonError> {
    if !path.exists() {
        return Ok(());
    }
    match UnixStream::connect(path).await {
        Ok(_) => {
            // Something is alive on the socket. This is unexpected after the
            // single-instance lock; fail clearly rather than clobbering it.
            Err(DaemonError::Socket {
                path: path.to_path_buf(),
                source: io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "a live daemon is already listening on this socket",
                ),
            })
        }
        Err(_) => {
            warn!(socket = %path.display(), "removing stale socket from a previous run");
            std::fs::remove_file(path).map_err(|source| DaemonError::Socket {
                path: path.to_path_buf(),
                source,
            })
        }
    }
}

/// Create `dir` (and parents) if missing and enforce `mode` on it.
fn ensure_dir_mode(dir: &Path, mode: u32) -> Result<(), DaemonError> {
    std::fs::create_dir_all(dir).map_err(|source| DaemonError::Directory {
        path: dir.to_path_buf(),
        source,
    })?;
    set_mode(dir, mode).map_err(|e| match e {
        DaemonError::Socket { source, .. } => DaemonError::Directory {
            path: dir.to_path_buf(),
            source,
        },
        other => other,
    })
}

/// Set a path's permission bits.
fn set_mode(path: &Path, mode: u32) -> Result<(), DaemonError> {
    let perms = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, perms).map_err(|source| DaemonError::Socket {
        path: path.to_path_buf(),
        source,
    })
}
