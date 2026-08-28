//! CLI compatibility wrapper around the public SDK client.

use std::time::Duration;

use protocol::{Method, Request};
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
        Self::connect_with_options(host, paths, pohunek_client::ClientOptions::default()).await
    }

    /// Connect with a custom daemon response timeout.
    pub(crate) async fn connect_with_request_timeout(
        host: &str,
        paths: &Paths,
        request_timeout: Duration,
    ) -> Result<Self, CliError> {
        let options =
            pohunek_client::ClientOptions::default().with_request_timeout(request_timeout);
        Self::connect_with_options(host, paths, options).await
    }

    async fn connect_with_options(
        host: &str,
        paths: &Paths,
        options: pohunek_client::ClientOptions,
    ) -> Result<Self, CliError> {
        let inner = pohunek_client::Client::connect_with_options(host, &paths.socket, options)
            .await
            .map_err(map_connect_error)?;
        Ok(Self { inner })
    }

    /// Send a request and await its response payload.
    pub(crate) async fn request(&mut self, request: &Request) -> Result<Value, CliError> {
        self.inner.request(request).await.map_err(map_client_error)
    }

    /// Send one typed SDK method request.
    pub(crate) async fn call<M>(&mut self, params: M::Params) -> Result<M::Output, CliError>
    where
        M: Method,
    {
        self.inner.call::<M>(params).await.map_err(map_client_error)
    }

    /// Convert this compatibility wrapper into the SDK client.
    pub(crate) fn into_sdk(self) -> pohunek_client::Client {
        self.inner
    }

    /// Open raw attach bytes on this client's selected route.
    pub(crate) async fn attach_raw(&self, stream_id: &str) -> Result<RawStream, CliError> {
        self.inner
            .attach_raw(stream_id)
            .await
            .map_err(map_connect_error)
    }
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
    fn preserves_local_descriptor_exhaustion_as_typed_sdk_error() {
        let err = map_connect_error(
            pohunek_client::ClientError::ClientFileDescriptorsExhausted {
                socket: PathBuf::from("/run/pohunek/daemon.sock"),
                source: io::Error::from_raw_os_error(libc::EMFILE),
            },
        );

        assert!(matches!(
            err,
            CliError::Client(pohunek_client::ClientError::ClientFileDescriptorsExhausted { .. })
        ));
    }

    #[test]
    fn maps_local_sdk_protocol_error_to_cli_protocol_error() {
        let err = map_client_error(pohunek_client::ClientError::Protocol(
            protocol::ProtocolError::method_not_found("assistant.materialize"),
        ));

        assert!(matches!(err, CliError::Protocol(source) if source.code == "method_not_found"));
    }
}
