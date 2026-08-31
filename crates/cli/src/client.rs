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

    #[cfg(test)]
    async fn connect_with_registry(
        host: &str,
        registry: pohunek_client::OverlayRegistry,
    ) -> Result<Self, CliError> {
        let inner =
            pohunek_client::Client::connect_with_registry(host, "/unused/local.sock", registry)
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
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::PathBuf;
    use std::sync::Arc;

    use overlay::{
        BindAddrError, ConfiguredTransport, DiscoveredPeer, ExternalIdentity, ExternalIdentityKind,
        OverlayError, OverlayFuture, OverlayId, OverlayRegistry, OverlayTransport, ResolvedPeer,
    };
    use pohunek_gui_core::{render_attach_command, AttachTemplateValues};
    use tokio::net::TcpListener;

    use super::*;

    #[derive(Debug)]
    struct RouteTransport {
        id: OverlayId,
        address: IpAddr,
    }

    impl RouteTransport {
        fn new(id: &str, address: IpAddr) -> Self {
            Self {
                id: OverlayId::new(id).expect("overlay id"),
                address,
            }
        }
    }

    impl OverlayTransport for RouteTransport {
        fn id(&self) -> &OverlayId {
            &self.id
        }

        fn validate_bind_addr(&self, addr: IpAddr) -> Result<(), BindAddrError> {
            if addr == self.address {
                Ok(())
            } else {
                Err(BindAddrError::NotMember(addr))
            }
        }

        fn listener_addr(&self) -> OverlayFuture<'_, IpAddr> {
            let address = self.address;
            Box::pin(async move { Ok(address) })
        }

        fn resolve_peer<'a>(&'a self, host: &'a str) -> OverlayFuture<'a, ResolvedPeer> {
            let result = if host == self.address.to_string() || host == "stable-peer" {
                Ok(ResolvedPeer {
                    peer_id: Some(format!("{}-peer", self.id)),
                    display_name: Some(format!("{} host", self.id)),
                    fqdn: None,
                    address: self.address,
                })
            } else {
                Err(OverlayError::HostUnknown {
                    host: host.to_owned(),
                    overlay: self.id.clone(),
                })
            };
            Box::pin(async move { result })
        }

        fn resolve_peer_identity<'a>(
            &'a self,
            identity: &'a ExternalIdentity,
        ) -> OverlayFuture<'a, ResolvedPeer> {
            let result = if identity.kind() == ExternalIdentityKind::PeerId
                && identity.value() == "stable-peer"
            {
                Ok(ResolvedPeer {
                    peer_id: Some(format!("{}-peer", self.id)),
                    display_name: Some(format!("{} host", self.id)),
                    fqdn: None,
                    address: self.address,
                })
            } else {
                Err(OverlayError::HostUnknown {
                    host: identity.value().to_owned(),
                    overlay: self.id.clone(),
                })
            };
            Box::pin(async move { result })
        }

        fn discover_peers(&self) -> OverlayFuture<'_, Vec<DiscoveredPeer>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    fn configured(id: &str, address: IpAddr, port: u16) -> ConfiguredTransport {
        ConfiguredTransport::new(Arc::new(RouteTransport::new(id, address)), port)
            .expect("configured route")
    }

    #[tokio::test]
    async fn gui_attach_route_preserves_discovered_port_over_cli_registry() {
        let address = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let registry_listener = TcpListener::bind(SocketAddr::new(address, 0))
            .await
            .expect("registry listener");
        let discovered_listener = TcpListener::bind(SocketAddr::new(address, 0))
            .await
            .expect("discovered listener");
        let registry_addr = registry_listener.local_addr().expect("registry address");
        let discovered_addr = discovered_listener
            .local_addr()
            .expect("discovered address");
        let registry =
            OverlayRegistry::new(vec![configured("policy", address, registry_addr.port())])
                .expect("registry");
        let identity = pohunek_client::ExternalIdentity::peer_id("stable-peer")
            .expect("stable identity")
            .selector();
        let route = pohunek_client::remote_host_with_port(
            &format!("policy:{identity}"),
            discovered_addr.port(),
        )
        .expect("discovered route");
        let command = render_attach_command(
            "{bin} attach --host {host} {id}",
            &AttachTemplateValues {
                bin: "pohunek".to_owned(),
                host: route,
                id: "s-42".to_owned(),
            },
        );
        let selector = command
            .split_whitespace()
            .skip_while(|part| *part != "--host")
            .nth(1)
            .expect("rendered host argument")
            .trim_matches('\'');

        let client = Client::connect_with_registry(selector, registry)
            .await
            .expect("CLI registry connection");
        let (_stream, _) =
            tokio::time::timeout(Duration::from_secs(1), discovered_listener.accept())
                .await
                .expect("discovered port accept deadline")
                .expect("discovered port accept");
        tokio::time::timeout(Duration::from_millis(25), registry_listener.accept())
            .await
            .expect_err("CLI registry port must not replace the discovered port");
        drop(client);
    }

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
