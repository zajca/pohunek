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

/// Deadline of a provider status probe when `overlays.toml` sets none.
pub const DEFAULT_STATUS_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// Smallest accepted configured status deadline, in seconds.
pub const MIN_STATUS_DEADLINE_SECS: u64 = 1;

/// Largest accepted configured status deadline, in seconds.
pub const MAX_STATUS_DEADLINE_SECS: u64 = 120;

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
        /// The hosts the selector matched; empty when the provider does not
        /// report them.
        candidates: Vec<CandidateIdentity>,
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
    #[error(
        "overlay {overlay} has no safe local listener address{}",
        reason_suffix(reason.as_deref())
    )]
    ListenerAddressMissing {
        /// Stable overlay identifier.
        overlay: OverlayId,
        /// Bounded, non-secret reason the provider gives for having no address,
        /// when it knows one.
        reason: Option<String>,
    },
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
            | Self::ListenerAddressMissing { overlay, .. } => overlay,
        }
    }
}

/// Renders an optional reason as the `: <reason>` tail of an error message.
fn reason_suffix(reason: Option<&str>) -> String {
    reason.map_or_else(String::new, |reason| format!(": {reason}"))
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
    /// A selector matched more than one host, on one overlay or across several.
    #[error(
        "host '{host}' matches {}{}",
        matched_hosts(candidates, *unlisted),
        describe_candidates(candidates)
    )]
    AmbiguousHost {
        /// User-provided host selector.
        host: String,
        /// Every host the selector matched that a provider listed.
        candidates: Vec<HostCandidate>,
        /// Whether a provider reported a collision without listing its hosts,
        /// so `candidates` is a lower bound.
        unlisted: bool,
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

/// One host a selector matched on an overlay, before the overlay is attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateIdentity {
    /// Whether the host is this machine rather than a peer.
    pub local: bool,
    /// Provider-qualified DNS name, falling back to the peer identity.
    pub name: String,
    /// Selector that names this host on its overlay, when one exists.
    pub selector: Option<String>,
    /// Whether `selector` is a provider peer identity, which no other peer
    /// shares. An FQDN selector can match a second peer with the same name.
    pub selector_is_peer_id: bool,
}

impl CandidateIdentity {
    /// This machine, named by its overlay FQDN when the provider reports one.
    ///
    /// Its selector is `local`: an overlay name can collide with a peer's
    /// again, `local` cannot.
    #[must_use]
    pub fn this_machine(fqdn: Option<&str>) -> Self {
        Self {
            local: true,
            name: fqdn.unwrap_or("unnamed").to_owned(),
            selector: Some("local".to_owned()),
            selector_is_peer_id: false,
        }
    }

    /// A peer, selected by its provider identity or else its FQDN.
    #[must_use]
    pub fn peer(peer_id: Option<&str>, fqdn: Option<&str>) -> Self {
        let by_id = peer_id.and_then(|id| ExternalIdentity::peer_id(id).ok());
        let selector_is_peer_id = by_id.is_some();
        let identity = by_id.or_else(|| fqdn.and_then(|name| ExternalIdentity::fqdn(name).ok()));
        Self {
            local: false,
            name: fqdn.or(peer_id).unwrap_or("unnamed").to_owned(),
            selector: identity.map(|identity| identity.selector()),
            selector_is_peer_id,
        }
    }
}

/// One host a selector matched, with the overlay that reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCandidate {
    /// Stable overlay identifier.
    pub overlay: OverlayId,
    /// The matched host.
    pub identity: CandidateIdentity,
}

impl HostCandidate {
    fn label(&self) -> String {
        if self.identity.local {
            format!("this machine ({})", self.identity.name)
        } else {
            format!("peer {}", self.identity.name)
        }
    }

    /// The selector that names only this host: `<overlay>:<selector>` for a
    /// peer, `local` for this machine, when one exists.
    #[must_use]
    pub fn qualified_selector(&self) -> Option<String> {
        let selector = self.identity.selector.as_ref()?;
        Some(if self.identity.local {
            selector.clone()
        } else {
            format!("{}:{selector}", self.overlay)
        })
    }
}

/// How many hosts an ambiguity names: the count when every candidate is listed
/// and there are at least two, otherwise `multiple hosts`.
fn matched_hosts(candidates: &[HostCandidate], unlisted: bool) -> String {
    match candidates.len() {
        count if count >= 2 && !unlisted => format!("{count} hosts"),
        _ => "multiple hosts".to_owned(),
    }
}

/// Describe `candidates` in plain words, as the tail of an ambiguity message.
///
/// Candidates on one overlay read ` on overlay netbird: this machine (a), peer
/// b`; across overlays each names its own overlay.
#[must_use]
pub fn describe_candidates(candidates: &[HostCandidate]) -> String {
    let Some(first) = candidates.first() else {
        return String::new();
    };
    if candidates.iter().all(|c| c.overlay == first.overlay) {
        let labels = candidates
            .iter()
            .map(HostCandidate::label)
            .collect::<Vec<_>>();
        format!(" on overlay {}: {}", first.overlay, labels.join(", "))
    } else {
        let labels = candidates
            .iter()
            .map(|c| format!("{} on overlay {}", c.label(), c.overlay))
            .collect::<Vec<_>>();
        format!(": {}", labels.join(", "))
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
    /// True when the provider listed this peer's address for another peer or
    /// for this machine too, so the address is withheld.
    pub address_ambiguous: bool,
}

/// Withholds every address that more than one peer lists.
///
/// An affected peer stays in the list as an address-less candidate with
/// [`DiscoveredPeer::address_ambiguous`] set, so no route is built from an
/// address that does not name one peer.
pub fn withhold_shared_addresses(peers: &mut [DiscoveredPeer]) {
    let shared = shared_addresses(peers.iter().filter_map(|peer| peer.address));
    for peer in peers {
        if peer
            .address
            .is_some_and(|address| shared.contains(&address))
        {
            peer.address = None;
            peer.address_ambiguous = true;
        }
    }
}

/// The addresses that occur more than once in `addresses`.
pub fn shared_addresses(addresses: impl IntoIterator<Item = IpAddr>) -> HashSet<IpAddr> {
    let mut seen = HashSet::new();
    let mut shared = HashSet::new();
    for address in addresses {
        if !seen.insert(address) {
            shared.insert(address);
        }
    }
    shared
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

/// What a host selector names on one overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedSelector {
    /// This machine itself, by its own overlay identity.
    LocalHost,
    /// One exact, policy-approved peer.
    Peer(ResolvedPeer),
}

/// This machine's own identity on one overlay, as its provider reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalIdentity {
    /// Stable overlay identifier.
    pub overlay: OverlayId,
    /// Provider-qualified DNS name, when reported.
    pub fqdn: Option<String>,
    /// Overlay address, when reported.
    pub address: Option<IpAddr>,
}

/// Where a host selector leads across all configured overlays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostTarget {
    /// The selector names this machine; the local daemon serves it.
    Local,
    /// The selector names one remote peer.
    Remote(OverlayRoute),
}

/// One provider read: the visible peers and this machine's own identity, both
/// taken from the same provider state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlaySnapshot {
    /// Visible remote peers, address-less candidates included.
    pub peers: Vec<DiscoveredPeer>,
    /// This machine's own identity, when the provider reports one.
    pub local: Option<LocalIdentity>,
}

/// [`OverlaySnapshot`] with the peers attached to their configured route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedSnapshot {
    /// Visible remote peers with their overlay routes.
    pub peers: Vec<RoutedPeer>,
    /// This machine's own identity, when the provider reports one.
    pub local: Option<LocalIdentity>,
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
    /// True when the provider withheld the address because another peer or this
    /// machine lists it too.
    pub address_ambiguous: bool,
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
    ///
    /// Providers must match only the identity field selected by
    /// [`ExternalIdentity::kind`]. They must not reinterpret the raw value as an
    /// untyped selector.
    fn resolve_peer_identity<'a>(
        &'a self,
        identity: &'a ExternalIdentity,
    ) -> OverlayFuture<'a, ResolvedPeer>;

    /// Enumerate visible remote peers while preserving address-less candidates.
    fn discover_peers(&self) -> OverlayFuture<'_, Vec<DiscoveredPeer>>;

    /// Read the visible peers and this machine's own identity in one pass.
    ///
    /// A provider that derives both from one state read overrides this, so the
    /// two cannot disagree and the state is read once. The default reports the
    /// peers only: a separate identity read could stall the peer enumeration
    /// that already finished, so a provider without the override contributes
    /// no local host record.
    fn discover_snapshot(&self) -> OverlayFuture<'_, OverlaySnapshot> {
        Box::pin(async move {
            let peers = self.discover_peers().await?;
            Ok(OverlaySnapshot { peers, local: None })
        })
    }

    /// Resolve a user selector to this machine or to one exact peer.
    ///
    /// A provider that knows its own identity matches it at the same steps as a
    /// peer's, so a name both carry is a [`OverlayError::PeerCollision`]. A
    /// provider without that knowledge resolves only peers.
    fn resolve_selector<'a>(&'a self, host: &'a str) -> OverlayFuture<'a, ResolvedSelector> {
        Box::pin(async move { self.resolve_peer(host).await.map(ResolvedSelector::Peer) })
    }

    /// Return this machine's own identity on the overlay, when the provider
    /// reports one.
    fn local_identity(&self) -> OverlayFuture<'_, Option<LocalIdentity>> {
        Box::pin(async { Ok(None) })
    }
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
        self.transport
            .discover_peers()
            .await
            .map(|peers| self.route_peers(peers))
    }

    /// Enumerate the peers and this machine's own identity from one provider
    /// read.
    ///
    /// # Errors
    ///
    /// Returns the provider's typed discovery error unchanged.
    pub async fn discover_snapshot(&self) -> Result<RoutedSnapshot, OverlayError> {
        let snapshot = self.transport.discover_snapshot().await?;
        Ok(RoutedSnapshot {
            peers: self.route_peers(snapshot.peers),
            local: snapshot.local,
        })
    }

    fn route_peers(&self, peers: Vec<DiscoveredPeer>) -> Vec<RoutedPeer> {
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
                address_ambiguous: peer.address_ambiguous,
                port: self.port(),
            })
            .collect()
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
        let mut collisions = Vec::new();
        let mut collided = false;
        let mut unlisted = false;
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
                    collided = true;
                    let listed = collision_candidates(&overlay, &error);
                    unlisted |= listed.is_empty();
                    collisions.extend(listed);
                    failures.push(OverlayFailure { overlay, error });
                }
                Err(error) => failures.push(OverlayFailure { overlay, error }),
            }
        }
        if collided {
            collisions.extend(routes.iter().map(route_candidate));
            return Err(RegistryError::AmbiguousHost {
                host: host.to_owned(),
                candidates: collisions,
                unlisted,
            });
        }
        match routes.len() {
            1 => Ok(routes.remove(0)),
            count if count > 1 => Err(RegistryError::AmbiguousHost {
                host: host.to_owned(),
                candidates: routes.iter().map(route_candidate).collect(),
                unlisted: false,
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
            Err(error @ OverlayError::PeerCollision { .. }) => {
                let candidates = collision_candidates(&overlay, &error);
                Err(RegistryError::AmbiguousHost {
                    host: host.to_owned(),
                    unlisted: candidates.is_empty(),
                    candidates,
                })
            }
            Err(error) => Err(RegistryError::HostUnavailable {
                host: host.to_owned(),
                failures: vec![OverlayFailure { overlay, error }],
            }),
        }
    }

    /// Resolve one unqualified or provider-qualified host to this machine or to
    /// one remote peer.
    ///
    /// Providers match their own identity next to their peers. A selector that
    /// names this machine on one overlay and a peer on another is ambiguous; one
    /// that names only this machine, on any number of overlays, is local. A
    /// typed identity selector (`<overlay>:peer_id:…`, `<overlay>:fqdn:…`)
    /// resolves peers only, as in [`Self::resolve_host`].
    ///
    /// # Errors
    ///
    /// Returns the typed unknown, ambiguous, or aggregated provider failures of
    /// [`Self::resolve_host`].
    pub async fn resolve_target(&self, host: &str) -> Result<HostTarget, RegistryError> {
        if host.parse::<IpAddr>().is_err() && host.contains(':') {
            return self.resolve_qualified_target(host).await;
        }

        let mut local_overlays = Vec::new();
        let mut routes = Vec::new();
        let mut failures = Vec::new();
        let mut collisions = Vec::new();
        let mut collided = false;
        let mut unlisted = false;
        let results = join_all(self.entries().iter().map(|entry| async move {
            (
                entry.id().clone(),
                entry.port(),
                entry.transport.resolve_selector(host).await,
            )
        }))
        .await;
        for (overlay, port, result) in results {
            match result {
                Ok(ResolvedSelector::LocalHost) => local_overlays.push(overlay),
                Ok(ResolvedSelector::Peer(peer)) => routes.push(OverlayRoute {
                    overlay,
                    peer_id: peer.peer_id,
                    display_name: peer.display_name,
                    fqdn: peer.fqdn,
                    addr: SocketAddr::new(peer.address, port),
                }),
                Err(OverlayError::HostUnknown { .. }) => {}
                Err(error @ OverlayError::PeerCollision { .. }) => {
                    collided = true;
                    let listed = collision_candidates(&overlay, &error);
                    unlisted |= listed.is_empty();
                    collisions.extend(listed);
                    failures.push(OverlayFailure { overlay, error });
                }
                Err(error) => failures.push(OverlayFailure { overlay, error }),
            }
        }
        if collided || (!local_overlays.is_empty() && !routes.is_empty()) {
            // Named by the selector: the error path starts no further provider
            // I/O, which a timeout could turn into a different outcome.
            collisions.extend(local_overlays.into_iter().map(|overlay| HostCandidate {
                overlay,
                identity: CandidateIdentity::this_machine(Some(host)),
            }));
            collisions.extend(routes.iter().map(route_candidate));
            return Err(RegistryError::AmbiguousHost {
                host: host.to_owned(),
                candidates: collisions,
                unlisted,
            });
        }
        if !local_overlays.is_empty() {
            return Ok(HostTarget::Local);
        }
        match routes.len() {
            1 => Ok(HostTarget::Remote(routes.remove(0))),
            count if count > 1 => Err(RegistryError::AmbiguousHost {
                host: host.to_owned(),
                candidates: routes.iter().map(route_candidate).collect(),
                unlisted: false,
            }),
            _ if failures.is_empty() => Err(RegistryError::HostUnknown(host.to_owned())),
            _ => Err(RegistryError::HostUnavailable {
                host: host.to_owned(),
                failures,
            }),
        }
    }

    async fn resolve_qualified_target(&self, host: &str) -> Result<HostTarget, RegistryError> {
        let (overlay, selector) = host
            .split_once(':')
            .filter(|(overlay, selector)| !overlay.is_empty() && !selector.is_empty())
            .ok_or_else(|| RegistryError::InvalidQualifiedHost(host.to_owned()))?;
        let overlay = OverlayId::new(overlay)
            .map_err(|_invalid_id| RegistryError::InvalidQualifiedHost(host.to_owned()))?;
        if ExternalIdentity::parse_selector(selector)?.is_some() {
            return self
                .resolve_qualified_host(host)
                .await
                .map(HostTarget::Remote);
        }
        let entry = self
            .entries()
            .iter()
            .find(|entry| entry.id() == &overlay)
            .ok_or_else(|| RegistryError::OverlayNotConfigured(overlay.clone()))?;
        match entry.transport.resolve_selector(selector).await {
            Ok(ResolvedSelector::LocalHost) => Ok(HostTarget::Local),
            Ok(ResolvedSelector::Peer(peer)) => Ok(HostTarget::Remote(OverlayRoute {
                overlay,
                peer_id: peer.peer_id,
                display_name: peer.display_name,
                fqdn: peer.fqdn,
                addr: SocketAddr::new(peer.address, entry.port()),
            })),
            Err(OverlayError::HostUnknown { .. }) => {
                Err(RegistryError::HostUnknown(host.to_owned()))
            }
            Err(error @ OverlayError::PeerCollision { .. }) => {
                let candidates = collision_candidates(&overlay, &error);
                Err(RegistryError::AmbiguousHost {
                    host: host.to_owned(),
                    unlisted: candidates.is_empty(),
                    candidates,
                })
            }
            Err(error) => Err(RegistryError::HostUnavailable {
                host: host.to_owned(),
                failures: vec![OverlayFailure { overlay, error }],
            }),
        }
    }

    /// This machine's identity on every configured overlay that reports one.
    ///
    /// An overlay whose provider fails or reports nothing is left out.
    pub async fn local_identities(&self) -> Vec<LocalIdentity> {
        join_all(
            self.entries()
                .iter()
                .map(|entry| async move { entry.transport.local_identity().await }),
        )
        .await
        .into_iter()
        .filter_map(|result| result.ok().flatten())
        .collect()
    }

    /// Collect one typed diagnostic per configured overlay.
    pub async fn diagnostics(&self) -> Vec<OverlayDiagnostic> {
        join_all(self.entries().iter().map(ConfiguredTransport::diagnostic)).await
    }
}

// Rust guideline compliant 2026-08-31

fn route_candidate(route: &OverlayRoute) -> HostCandidate {
    HostCandidate {
        overlay: route.overlay.clone(),
        identity: CandidateIdentity::peer(route.peer_id.as_deref(), route.fqdn.as_deref()),
    }
}

/// The hosts a provider's collision error names; empty when the provider
/// reports none, so the ambiguity keeps its unknown candidate set.
fn collision_candidates(overlay: &OverlayId, error: &OverlayError) -> Vec<HostCandidate> {
    let reported = match error {
        OverlayError::PeerCollision { candidates, .. } => candidates.as_slice(),
        _ => &[],
    };
    reported
        .iter()
        .map(|identity| HostCandidate {
            overlay: overlay.clone(),
            identity: identity.clone(),
        })
        .collect()
}
