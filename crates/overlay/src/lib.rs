//! Generic overlay transports and configured routing.
//!
//! Providers implement [`OverlayTransport`]. Applications compose those
//! implementations through [`OverlayRegistry`], which owns per-overlay ports,
//! rejects duplicate identifiers, aggregates discovery, and resolves ambiguous
//! names fail-closed.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU16;
use std::pin::Pin;
use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use futures::future::join_all;

const PEER_IDENTITY_PREFIX: &str = "peer~";
const FQDN_IDENTITY_PREFIX: &str = "fqdn~";

/// Why an overlay bind address was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BindAddrError {
    /// The address is outside the provider's current trusted membership.
    #[error("bind address {0} is not a current overlay member address")]
    NotMember(IpAddr),
    /// The address category is never safe for a production listener.
    #[error("bind address {0} is forbidden for an overlay listener")]
    Forbidden(IpAddr),
}

/// Errors returned by one overlay provider.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum OverlayError {
    /// The requested host is not present in current provider state.
    #[error("host '{host}' was not found in overlay {overlay}")]
    HostUnknown {
        /// User-provided host selector.
        host: String,
        /// Stable overlay identifier.
        overlay: OverlayId,
    },
    /// More than one current peer matches the selector.
    #[error("host '{host}' is ambiguous inside overlay {overlay}")]
    PeerCollision {
        /// User-provided host selector.
        host: String,
        /// Stable overlay identifier.
        overlay: OverlayId,
    },
    /// The provider CLI is unavailable.
    #[error("overlay CLI for {0} is not installed")]
    CliMissing(OverlayId),
    /// Provider state could not be loaded or parsed.
    #[error("overlay {overlay} state unavailable: {detail}")]
    StateUnavailable {
        /// Stable overlay identifier.
        overlay: OverlayId,
        /// Bounded, non-secret provider detail.
        detail: String,
    },
    /// Provider configuration is invalid.
    #[error("overlay {overlay} configuration is invalid: {detail}")]
    InvalidConfig {
        /// Stable overlay identifier.
        overlay: OverlayId,
        /// Bounded, non-secret configuration detail.
        detail: String,
    },
    /// Current provider state has no safe local listener address.
    #[error("overlay {0} has no safe local listener address")]
    ListenerAddressMissing(OverlayId),
}

impl OverlayError {
    /// Return the overlay associated with this error.
    #[must_use]
    pub fn overlay(&self) -> &OverlayId {
        match self {
            Self::HostUnknown { overlay, .. }
            | Self::PeerCollision { overlay, .. }
            | Self::StateUnavailable { overlay, .. }
            | Self::InvalidConfig { overlay, .. }
            | Self::CliMissing(overlay)
            | Self::ListenerAddressMissing(overlay) => overlay,
        }
    }
}

/// Errors raised while constructing or querying a registry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RegistryError {
    /// No transport was configured.
    #[error("at least one overlay transport must be configured")]
    Empty,
    /// A stable overlay identifier was syntactically invalid.
    #[error(
        "invalid overlay identifier '{value}': use lowercase ASCII letters, digits, or hyphens"
    )]
    InvalidId {
        /// Rejected identifier value.
        value: String,
    },
    /// Two configured transports claimed the same identifier.
    #[error("overlay '{0}' is configured more than once")]
    DuplicateId(OverlayId),
    /// A configured listener port was zero.
    #[error("overlay '{0}' has invalid port 0")]
    InvalidPort(OverlayId),
    /// A provider-qualified host omitted its provider or selector.
    #[error("invalid provider-qualified host '{0}': expected '<overlay>:<selector>'")]
    InvalidQualifiedHost(String),
    /// A canonical external identity was malformed or empty.
    #[error("invalid external peer identity '{selector}': {detail}")]
    InvalidExternalIdentity {
        /// Rejected canonical selector.
        selector: String,
        /// Stable, non-secret validation detail.
        detail: &'static str,
    },
    /// A provider-qualified host named an overlay that is not configured.
    #[error("overlay '{0}' is not configured")]
    OverlayNotConfigured(OverlayId),
    /// More than one healthy overlay resolved the same unqualified host.
    #[error("host '{host}' is ambiguous across overlays: {overlays:?}")]
    AmbiguousHost {
        /// User-provided host selector.
        host: String,
        /// Healthy overlays that resolved the selector.
        overlays: Vec<OverlayId>,
    },
    /// No configured overlay knows the requested host.
    #[error("host '{0}' was not found in any configured overlay")]
    HostUnknown(String),
    /// No healthy overlay resolved the host and at least one provider failed.
    #[error("no healthy overlay could resolve host '{host}'")]
    HostUnavailable {
        /// User-provided host selector.
        host: String,
        /// Per-overlay typed failures retained for diagnostics.
        failures: Vec<OverlayFailure>,
    },
}

/// Provider identity kind retained by a canonical external selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalIdentityKind {
    /// Stable provider peer identifier.
    PeerId,
    /// Provider-qualified DNS name used only when no peer identifier exists.
    Fqdn,
}

/// Slash-safe provider identity used in CLI targets and relay paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalIdentity {
    kind: ExternalIdentityKind,
    value: String,
}

impl ExternalIdentity {
    /// Construct a stable provider peer identity.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::InvalidExternalIdentity`] when `value` is empty.
    pub fn peer_id(value: impl Into<String>) -> Result<Self, RegistryError> {
        Self::new(ExternalIdentityKind::PeerId, value.into())
    }

    /// Construct an FQDN fallback identity.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::InvalidExternalIdentity`] when `value` is empty.
    pub fn fqdn(value: impl Into<String>) -> Result<Self, RegistryError> {
        Self::new(ExternalIdentityKind::Fqdn, value.into())
    }

    fn new(kind: ExternalIdentityKind, value: String) -> Result<Self, RegistryError> {
        if value.is_empty() {
            return Err(RegistryError::InvalidExternalIdentity {
                selector: String::new(),
                detail: "the decoded identity must be non-empty",
            });
        }
        Ok(Self { kind, value })
    }

    /// Return the provider identity kind.
    #[must_use]
    pub fn kind(&self) -> ExternalIdentityKind {
        self.kind
    }

    /// Return the decoded provider value.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Encode this identity as a slash-safe canonical selector.
    #[must_use]
    pub fn selector(&self) -> String {
        let prefix = match self.kind {
            ExternalIdentityKind::PeerId => PEER_IDENTITY_PREFIX,
            ExternalIdentityKind::Fqdn => FQDN_IDENTITY_PREFIX,
        };
        format!("{prefix}{}", URL_SAFE_NO_PAD.encode(self.value.as_bytes()))
    }

    fn parse_selector(selector: &str) -> Result<Option<Self>, RegistryError> {
        let (kind, encoded) = if let Some(encoded) = selector.strip_prefix(PEER_IDENTITY_PREFIX) {
            (ExternalIdentityKind::PeerId, encoded)
        } else if let Some(encoded) = selector.strip_prefix(FQDN_IDENTITY_PREFIX) {
            (ExternalIdentityKind::Fqdn, encoded)
        } else {
            return Ok(None);
        };
        if encoded.is_empty() {
            return Err(RegistryError::InvalidExternalIdentity {
                selector: selector.to_owned(),
                detail: "the encoded identity must be non-empty",
            });
        }
        let bytes = URL_SAFE_NO_PAD.decode(encoded).map_err(|_error| {
            RegistryError::InvalidExternalIdentity {
                selector: selector.to_owned(),
                detail: "expected unpadded base64url after the identity prefix",
            }
        })?;
        if URL_SAFE_NO_PAD.encode(&bytes) != encoded {
            return Err(RegistryError::InvalidExternalIdentity {
                selector: selector.to_owned(),
                detail: "expected canonical unpadded base64url",
            });
        }
        let value =
            String::from_utf8(bytes).map_err(|_error| RegistryError::InvalidExternalIdentity {
                selector: selector.to_owned(),
                detail: "the decoded identity must be UTF-8",
            })?;
        Self::new(kind, value).map(Some)
    }
}

/// One provider failure retained by registry aggregation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayFailure {
    /// Stable overlay identifier.
    pub overlay: OverlayId,
    /// Provider-specific typed error.
    pub error: OverlayError,
}

/// Stable machine identifier for an overlay transport.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OverlayId(Arc<str>);

impl OverlayId {
    /// Validate and construct an overlay identifier.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::InvalidId`] for empty values or characters
    /// outside lowercase ASCII letters, digits, and hyphens.
    pub fn new(value: impl Into<String>) -> Result<Self, RegistryError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
        if !valid {
            return Err(RegistryError::InvalidId { value });
        }
        Ok(Self(value.into()))
    }

    /// Return the identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OverlayId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A peer discovered through one overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredPeer {
    /// Provider peer identity, when the provider supplies one.
    pub peer_id: Option<String>,
    /// Human-friendly short name, when available.
    pub display_name: Option<String>,
    /// Provider-qualified DNS name, when available.
    pub fqdn: Option<String>,
    /// Policy-approved dial address, or `None` for an address-less candidate.
    pub address: Option<IpAddr>,
}

/// One provider-resolved peer address before registry routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPeer {
    /// Provider peer identity, when available.
    pub peer_id: Option<String>,
    /// Human-friendly short name, when available.
    pub display_name: Option<String>,
    /// Provider-qualified DNS name, when available.
    pub fqdn: Option<String>,
    /// Exact policy-approved address.
    pub address: IpAddr,
}

/// A discovered peer with its configured overlay route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedPeer {
    /// Stable overlay identifier.
    pub overlay: OverlayId,
    /// Provider peer identity, when available.
    pub peer_id: Option<String>,
    /// Human-friendly short name, when available.
    pub display_name: Option<String>,
    /// Provider-qualified DNS name, when available.
    pub fqdn: Option<String>,
    /// Exact socket route, or `None` for an address-less candidate.
    pub addr: Option<SocketAddr>,
    /// Configured daemon port, including for address-less candidates.
    pub port: u16,
}

/// A host resolved to one exact overlay route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayRoute {
    /// Stable overlay identifier.
    pub overlay: OverlayId,
    /// Provider peer identity, when available.
    pub peer_id: Option<String>,
    /// Human-friendly short name, when available.
    pub display_name: Option<String>,
    /// Provider-qualified DNS name, when available.
    pub fqdn: Option<String>,
    /// Exact socket address including the overlay-specific port.
    pub addr: SocketAddr,
}

/// Current diagnostic state for one configured overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayDiagnostic {
    /// Stable overlay identifier.
    pub overlay: OverlayId,
    /// Configured daemon port.
    pub port: u16,
    /// Safe local listener address when provider state is ready.
    pub listener: Option<SocketAddr>,
    /// Typed provider error when diagnostics are unavailable.
    pub error: Option<OverlayError>,
}

/// Provider operation future that remains cancellable when dropped.
pub type OverlayFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, OverlayError>> + Send + 'a>>;

/// Contract implemented by every overlay provider.
pub trait OverlayTransport: Send + Sync + fmt::Debug {
    /// Return the stable machine identifier for this provider.
    fn id(&self) -> &OverlayId;

    /// Validate a daemon listener address against current provider policy.
    fn validate_bind_addr(&self, addr: IpAddr) -> Result<(), BindAddrError>;

    /// Return this host's current safe listener address.
    fn listener_addr(&self) -> OverlayFuture<'_, IpAddr>;

    /// Resolve a user selector to one exact, policy-approved peer.
    fn resolve_peer<'a>(&'a self, host: &'a str) -> OverlayFuture<'a, ResolvedPeer>;

    /// Resolve one typed canonical identity to an exact current peer.
    fn resolve_peer_identity<'a>(
        &'a self,
        identity: &'a ExternalIdentity,
    ) -> OverlayFuture<'a, ResolvedPeer> {
        self.resolve_peer(identity.value())
    }

    /// Enumerate visible remote peers while preserving address-less candidates.
    fn discover_peers(&self) -> OverlayFuture<'_, Vec<DiscoveredPeer>>;
}

/// One transport paired with its configured daemon port.
#[derive(Clone)]
pub struct ConfiguredTransport {
    transport: Arc<dyn OverlayTransport>,
    port: NonZeroU16,
}

impl ConfiguredTransport {
    /// Pair a transport with a non-zero port.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::InvalidPort`] when `port` is zero.
    pub fn new(transport: Arc<dyn OverlayTransport>, port: u16) -> Result<Self, RegistryError> {
        let id = transport.id().clone();
        let port = NonZeroU16::new(port).ok_or(RegistryError::InvalidPort(id))?;
        Ok(Self { transport, port })
    }

    /// Return the stable overlay identifier.
    #[must_use]
    pub fn id(&self) -> &OverlayId {
        self.transport.id()
    }

    /// Return the configured daemon port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port.get()
    }

    /// Return the provider implementation.
    #[must_use]
    pub fn transport(&self) -> &Arc<dyn OverlayTransport> {
        &self.transport
    }

    /// Discover peers and apply this entry's route configuration.
    ///
    /// # Errors
    ///
    /// Returns the provider's typed discovery error unchanged.
    pub async fn discover_peers(&self) -> Result<Vec<RoutedPeer>, OverlayError> {
        self.transport.discover_peers().await.map(|peers| {
            peers
                .into_iter()
                .map(|peer| RoutedPeer {
                    overlay: self.id().clone(),
                    peer_id: peer.peer_id,
                    display_name: peer.display_name,
                    fqdn: peer.fqdn,
                    addr: peer
                        .address
                        .map(|address| SocketAddr::new(address, self.port())),
                    port: self.port(),
                })
                .collect()
        })
    }

    /// Return this entry's current listener diagnostic.
    pub async fn diagnostic(&self) -> OverlayDiagnostic {
        match self.transport.listener_addr().await {
            Ok(address) => OverlayDiagnostic {
                overlay: self.id().clone(),
                port: self.port(),
                listener: Some(SocketAddr::new(address, self.port())),
                error: None,
            },
            Err(error) => OverlayDiagnostic {
                overlay: self.id().clone(),
                port: self.port(),
                listener: None,
                error: Some(error),
            },
        }
    }
}

impl fmt::Debug for ConfiguredTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfiguredTransport")
            .field("overlay", self.id())
            .field("port", &self.port())
            .finish_non_exhaustive()
    }
}

/// Validated configured overlay registry.
#[derive(Debug, Clone)]
pub struct OverlayRegistry {
    entries: Arc<[ConfiguredTransport]>,
}

impl OverlayRegistry {
    /// Build a non-empty registry with unique overlay identifiers.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::Empty`] for no entries or
    /// [`RegistryError::DuplicateId`] when identifiers collide.
    pub fn new(entries: Vec<ConfiguredTransport>) -> Result<Self, RegistryError> {
        if entries.is_empty() {
            return Err(RegistryError::Empty);
        }
        let mut ids = HashSet::with_capacity(entries.len());
        for entry in &entries {
            if !ids.insert(entry.id().clone()) {
                return Err(RegistryError::DuplicateId(entry.id().clone()));
            }
        }
        Ok(Self {
            entries: entries.into(),
        })
    }

    /// Return configured entries in deterministic configuration order.
    #[must_use]
    pub fn entries(&self) -> &[ConfiguredTransport] {
        &self.entries
    }

    /// Resolve one unqualified or provider-qualified host.
    ///
    /// A provider-qualified selector uses `<overlay>:<selector>` and is sent
    /// only to that configured provider. Unqualified selectors are sent to all
    /// providers; this includes bare IPv6 literals. A unique healthy success
    /// wins and cross-overlay collisions fail closed.
    ///
    /// # Errors
    ///
    /// Returns typed unknown, ambiguous, or aggregated provider failures.
    pub async fn resolve_host(&self, host: &str) -> Result<OverlayRoute, RegistryError> {
        if host.parse::<IpAddr>().is_err() && host.contains(':') {
            return self.resolve_qualified_host(host).await;
        }

        let mut routes = Vec::new();
        let mut failures = Vec::new();
        let mut collision_overlays = Vec::new();
        let results = join_all(self.entries().iter().map(|entry| async move {
            (
                entry.id().clone(),
                entry.port(),
                entry.transport.resolve_peer(host).await,
            )
        }))
        .await;
        for (overlay, port, result) in results {
            match result {
                Ok(peer) => routes.push(OverlayRoute {
                    overlay,
                    peer_id: peer.peer_id,
                    display_name: peer.display_name,
                    fqdn: peer.fqdn,
                    addr: SocketAddr::new(peer.address, port),
                }),
                Err(OverlayError::HostUnknown { .. }) => {}
                Err(error @ OverlayError::PeerCollision { .. }) => {
                    collision_overlays.push(overlay.clone());
                    failures.push(OverlayFailure { overlay, error });
                }
                Err(error) => failures.push(OverlayFailure { overlay, error }),
            }
        }
        if !collision_overlays.is_empty() {
            collision_overlays.extend(routes.into_iter().map(|route| route.overlay));
            collision_overlays.sort();
            collision_overlays.dedup();
            return Err(RegistryError::AmbiguousHost {
                host: host.to_owned(),
                overlays: collision_overlays,
            });
        }
        match routes.len() {
            1 => Ok(routes.remove(0)),
            count if count > 1 => Err(RegistryError::AmbiguousHost {
                host: host.to_owned(),
                overlays: routes.into_iter().map(|route| route.overlay).collect(),
            }),
            _ if failures.is_empty() => Err(RegistryError::HostUnknown(host.to_owned())),
            _ => Err(RegistryError::HostUnavailable {
                host: host.to_owned(),
                failures,
            }),
        }
    }

    async fn resolve_qualified_host(&self, host: &str) -> Result<OverlayRoute, RegistryError> {
        let (overlay, selector) = host
            .split_once(':')
            .filter(|(overlay, selector)| !overlay.is_empty() && !selector.is_empty())
            .ok_or_else(|| RegistryError::InvalidQualifiedHost(host.to_owned()))?;
        let overlay = OverlayId::new(overlay)
            .map_err(|_invalid_id| RegistryError::InvalidQualifiedHost(host.to_owned()))?;
        let entry = self
            .entries()
            .iter()
            .find(|entry| entry.id() == &overlay)
            .ok_or_else(|| RegistryError::OverlayNotConfigured(overlay.clone()))?;

        let identity = ExternalIdentity::parse_selector(selector)?;
        let resolved = match identity.as_ref() {
            Some(identity) => entry.transport.resolve_peer_identity(identity).await,
            None => entry.transport.resolve_peer(selector).await,
        };
        match resolved {
            Ok(peer) => Ok(OverlayRoute {
                overlay,
                peer_id: peer.peer_id,
                display_name: peer.display_name,
                fqdn: peer.fqdn,
                addr: SocketAddr::new(peer.address, entry.port()),
            }),
            Err(OverlayError::HostUnknown { .. }) => {
                Err(RegistryError::HostUnknown(host.to_owned()))
            }
            Err(error @ OverlayError::PeerCollision { .. }) => Err(RegistryError::AmbiguousHost {
                host: host.to_owned(),
                overlays: vec![error.overlay().clone()],
            }),
            Err(error) => Err(RegistryError::HostUnavailable {
                host: host.to_owned(),
                failures: vec![OverlayFailure { overlay, error }],
            }),
        }
    }

    /// Collect one typed diagnostic per configured overlay.
    pub async fn diagnostics(&self) -> Vec<OverlayDiagnostic> {
        join_all(self.entries().iter().map(ConfiguredTransport::diagnostic)).await
    }
}

// Rust guideline compliant 2026-08-28
