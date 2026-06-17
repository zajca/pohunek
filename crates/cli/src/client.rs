//! Local control-protocol client.
//!
//! Connects to the daemon's Unix socket and performs a single request/response
//! exchange using newline-delimited JSON (the shared `protocol` crate). The CLI
//! is host-aware in its argument parsing (see [`crate::target`]) but only the
//! local transport exists in Phase 1; remote targets are rejected with a typed
//! error before reaching here.

use std::path::Path;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use protocol::{Request, Response};
use serde_json::Value;
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LinesCodec, LinesCodecError};

use crate::error::CliError;

/// Default per-request timeout. Bounds the CLI when the daemon is wedged.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Mirrors the daemon's max control-line length.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// A connected control client over the local Unix socket.
#[derive(Debug)]
pub(crate) struct LocalClient {
    framed: Framed<UnixStream, LinesCodec>,
}

impl LocalClient {
    /// Connect to the daemon control socket at `socket_path`.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::DaemonUnreachable`] when the socket cannot be dialed
    /// (typically: the daemon is not running).
    pub(crate) async fn connect(socket_path: &Path) -> Result<Self, CliError> {
        let stream = UnixStream::connect(socket_path)
            .await
            .map_err(|source| CliError::DaemonUnreachable {
                socket: socket_path.to_path_buf(),
                source,
            })?;
        let codec = LinesCodec::new_with_max_length(MAX_LINE_BYTES);
        Ok(Self {
            framed: Framed::new(stream, codec),
        })
    }

    /// Send a request and await its response, applying the request timeout.
    ///
    /// Returns the daemon's `ok` payload, or maps a daemon error response to a
    /// typed [`CliError::Protocol`].
    ///
    /// # Errors
    ///
    /// Connection, framing, timeout, or daemon-side protocol errors.
    pub(crate) async fn request(&mut self, request: &Request) -> Result<Value, CliError> {
        let line = serde_json::to_string(request)?;

        tokio::time::timeout(REQUEST_TIMEOUT, self.exchange(line))
            .await
            .map_err(|_| {
                CliError::Framing("timed out waiting for daemon response".to_owned())
            })?
    }

    /// Perform one line send + one line receive.
    async fn exchange(&mut self, line: String) -> Result<Value, CliError> {
        self.framed
            .send(line)
            .await
            .map_err(map_codec_err)?;

        let reply = self
            .framed
            .next()
            .await
            .ok_or_else(|| CliError::Framing("daemon closed the connection without a response".to_owned()))?
            .map_err(map_codec_err)?;

        let response: Response = serde_json::from_str(&reply)?;
        match response {
            Response::Ok { ok, .. } => Ok(ok),
            Response::Err { err, .. } => Err(CliError::Protocol(err)),
        }
    }
}

fn map_codec_err(err: LinesCodecError) -> CliError {
    match err {
        LinesCodecError::Io(io) => CliError::Io(io),
        LinesCodecError::MaxLineLengthExceeded => {
            CliError::Framing("control line exceeded maximum length".to_owned())
        }
    }
}
