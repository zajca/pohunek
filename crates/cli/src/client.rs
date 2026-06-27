//! CLI compatibility wrapper around the public SDK client.

use protocol::Request;
use serde_json::Value;

use crate::error::CliError;
use crate::paths::Paths;

pub(crate) use pohunek_client::RawStream;

/// A connected control client used by CLI commands.
#[derive(Debug)]
pub(crate) struct Client {
    inner: pohunek_client::Client,
}

impl Client {
    /// Connect to the daemon for `host`.
    pub(crate) async fn connect(host: &str, paths: &Paths) -> Result<Self, CliError> {
        let inner = pohunek_client::Client::connect(host, &paths.socket)
            .await
            .map_err(map_connect_error)?;
        Ok(Self { inner })
    }

    /// Send a request and await its response payload.
    pub(crate) async fn request(&mut self, request: &Request) -> Result<Value, CliError> {
        self.inner.request(request).await.map_err(map_client_error)
    }

    /// Convert this compatibility wrapper into the SDK client.
    pub(crate) fn into_sdk(self) -> pohunek_client::Client {
        self.inner
    }
}

/// Open a raw, unframed control connection for `host`.
pub(crate) async fn connect_raw(host: &str, paths: &Paths) -> Result<RawStream, CliError> {
    pohunek_client::connect_raw(host, &paths.socket)
        .await
        .map_err(map_connect_error)
}

fn map_connect_error(err: pohunek_client::ClientError) -> CliError {
    match err {
        pohunek_client::ClientError::DaemonUnreachable { socket, source } => {
            CliError::DaemonUnreachable { socket, source }
        }
        other => map_client_error(other),
    }
}

fn map_client_error(err: pohunek_client::ClientError) -> CliError {
    match err {
        pohunek_client::ClientError::Protocol(source) => CliError::Protocol(source),
        other => CliError::Client(other),
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn maps_sdk_daemon_unreachable_to_cli_bootstrap_error() {
        let err = map_connect_error(pohunek_client::ClientError::DaemonUnreachable {
            socket: PathBuf::from("/run/pohunek/daemon.sock"),
            source: io::Error::new(io::ErrorKind::NotFound, "missing"),
        });

        assert!(matches!(err, CliError::DaemonUnreachable { .. }));
    }

    #[test]
    fn maps_local_sdk_protocol_error_to_cli_protocol_error() {
        let err = map_client_error(pohunek_client::ClientError::Protocol(
            protocol::ProtocolError::method_not_found("assistant.materialize"),
        ));

        assert!(matches!(err, CliError::Protocol(source) if source.code == "method_not_found"));
    }
}
