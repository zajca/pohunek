//! Overlay provider configuration, read from `<config>/pohunek/overlays.toml`.
//!
//! The daemon, the CLI and external Rust clients share this loader, so one
//! owner-private file decides which overlay a host works over and which TCP
//! port the remote control listener uses. The file is optional: when it or its
//! directory is absent, the host works over `NetBird` on [`DEFAULT_REMOTE_PORT`].
//!
//! When nothing exists at the file path (checked without following a symlink at
//! the file itself), there is no configuration to trust: the defaults apply and
//! the directory is not inspected, so a symlinked or group-writable
//! `~/.config` never blocks a host that did not opt in. A writer in an unsafe
//! directory can then only make a file appear, which is refused by the checks
//! below, or delete it, which falls back to the `NetBird` default.
//!
//! When anything exists at that path (file, symlink, FIFO), every component of
//! the config directory path is opened without following symlinks and must be
//! owned by the user (or by root and not writable) and not be writable by other
//! accounts, and the file must be a regular file owned by the user with mode
//! `0600`. Parsing is strict: unknown keys, wrong types and out-of-range values
//! fail with [`ClientError::OverlayConfiguration`] before any provider is
//! consulted. Error messages carry the file path and, at most, a line number,
//! never a value from the file.
//!
//! `status_timeout_secs` bounds every provider status probe; see
//! [`overlay::DEFAULT_STATUS_DEADLINE`] for the default.
//!
//! `POHUNEK_REMOTE_PORT`, when set, overrides the file's `port`. The file is
//! validated either way.

// Rust guideline compliant 2026-10-23

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::sync::Arc;
use std::time::Duration;

use futures::future::join_all;
use netbird::{NetbirdTransport, StaticPeer, StaticPeers, NETBIRD_OVERLAY_ID};
use overlay::{
    ConfiguredTransport, OverlayDiagnostic, OverlayError, OverlayRegistry, OverlayTransport,
    DEFAULT_STATUS_DEADLINE, MAX_STATUS_DEADLINE_SECS, MIN_STATUS_DEADLINE_SECS,
};
use overlay_tailscale::{TailscaleTransport, TAILSCALE_OVERLAY_ID};
use pohunek_platform::filesystem::{FsError, TrustedDir};
use serde::Deserialize;

use crate::ClientError;

/// File name of the overlay configuration in pohunek's config directory.
pub const OVERLAY_CONFIG_FILE: &str = "overlays.toml";

/// Default TCP port of the remote control listener.
///
/// Used when `overlays.toml` sets no `port` and [`REMOTE_PORT_ENV`] is unset.
/// Chosen below the Linux ephemeral range (`32768`+) so it does not collide
/// with outbound ephemeral source ports.
pub const DEFAULT_REMOTE_PORT: u16 = 18722;

/// Environment variable that overrides the configured remote control port.
pub const REMOTE_PORT_ENV: &str = "POHUNEK_REMOTE_PORT";

// Documented invariants, checked at compile time: the default port is non-zero
// and below the Linux default ephemeral floor (32768).
const _: () = assert!(DEFAULT_REMOTE_PORT > 0);
const _: () = assert!(DEFAULT_REMOTE_PORT < 32768);

/// Largest accepted configuration file, checked before parsing.
const MAX_FILE_BYTES: usize = 64 * 1024;

/// Required mode of the configuration file: owner read and write only, because
/// the file decides which network the control listener binds on.
const FILE_MODE: u32 = 0o600;

/// Directory mode bits that would let another account replace the file.
const FORBIDDEN_DIRECTORY_BITS: u32 = 0o022;

/// Overlay ids the loader can build a provider for.
const KNOWN_PROVIDER_IDS: [&str; 2] = [NETBIRD_OVERLAY_ID, TAILSCALE_OVERLAY_ID];

/// Largest number of static peers one `overlays.toml` may list.
const MAX_STATIC_PEERS: usize = 32;

/// The CGNAT range `NetBird` assigns addresses from, as an IPv4 network and prefix.
const NETBIRD_CGNAT: (Ipv4Addr, u8) = (Ipv4Addr::new(100, 64, 0, 0), 10);

/// A provider the loader knows how to construct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provider {
    Netbird,
    Tailscale,
}

impl Provider {
    fn from_id(id: &str) -> Option<Self> {
        match id {
            NETBIRD_OVERLAY_ID => Some(Self::Netbird),
            TAILSCALE_OVERLAY_ID => Some(Self::Tailscale),
            _ => None,
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Netbird => NETBIRD_OVERLAY_ID,
            Self::Tailscale => TAILSCALE_OVERLAY_ID,
        }
    }

    fn transport(
        self,
        status_deadline: Duration,
        static_peers: Option<StaticPeers>,
    ) -> Arc<dyn OverlayTransport> {
        match self {
            Self::Netbird => Arc::new(NetbirdTransport::with_static_peers(
                status_deadline,
                static_peers,
            )),
            Self::Tailscale => Arc::new(TailscaleTransport::with_status_deadline(status_deadline)),
        }
    }
}

/// Issue that lifts the one-overlay-per-host limit, named in the error.
const MULTI_OVERLAY_ISSUE: &str = "#1113";

/// One `providers` entry. A non-string entry is kept as such, so the entry
/// count is checked before the entries are.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Entry {
    Id(String),
    Other(serde::de::IgnoredAny),
}

/// Raw shape of `overlays.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    providers: Vec<Entry>,
    /// Wide on purpose: an out-of-range number reports the accepted range
    /// instead of a bare type error.
    port: Option<i64>,
    /// Wide on purpose, like `port`.
    status_timeout_secs: Option<i64>,
    netbird: Option<RawNetbird>,
}

/// Raw shape of the `[netbird]` table.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNetbird {
    subnet: String,
    static_peers: BTreeMap<String, RawStaticPeer>,
}

/// Raw shape of one `[netbird.static_peers.<name>]` entry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStaticPeer {
    address: String,
    fqdn: String,
    host_id: String,
}

/// The overlay providers a host works over and the shared remote port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayConfig {
    providers: Vec<Provider>,
    port: u16,
    status_deadline: Duration,
    static_peers: Option<StaticPeers>,
    /// `true` when the providers came from `overlays.toml`, `false` for the
    /// `NetBird` default of an absent file.
    from_file: bool,
}

impl OverlayConfig {
    /// Ids of the configured overlay providers, in file order.
    #[must_use]
    pub fn providers(&self) -> Vec<&'static str> {
        self.providers
            .iter()
            .map(|provider| provider.id())
            .collect()
    }

    /// True when the providers came from `overlays.toml`, false for the
    /// `NetBird` default of an absent file.
    ///
    /// A provider the owner listed is expected to appear, so the daemon keeps
    /// retrying while its CLI is missing; the default stays disabled then.
    #[must_use]
    pub fn from_file(&self) -> bool {
        self.from_file
    }

    /// TCP port of the remote control listener on every configured overlay.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Longest a provider status probe may run before it is killed and the
    /// provider is reported unavailable.
    #[must_use]
    pub fn status_deadline(&self) -> Duration {
        self.status_deadline
    }

    /// Load the configuration from the XDG config directory of this process.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::OverlayConfiguration`] when the config directory
    /// cannot be resolved or [`OverlayConfig::load_from`] fails.
    pub fn load() -> Result<Self, ClientError> {
        Self::load_from(&config_dir()?)
    }

    /// Load the configuration from `config_dir`, then apply [`REMOTE_PORT_ENV`].
    ///
    /// An absent file or directory yields `NetBird` on [`DEFAULT_REMOTE_PORT`].
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::OverlayConfiguration`] when the file or its
    /// directory is unsafe, unreadable, too large, not UTF-8, not TOML of the
    /// schema, or holds an unsupported value, and when [`REMOTE_PORT_ENV`] is
    /// set to an unusable value.
    pub fn load_from(config_dir: &Path) -> Result<Self, ClientError> {
        let path = config_dir.join(OVERLAY_CONFIG_FILE);
        let invalid = |reason: String| ClientError::OverlayConfiguration {
            path: path.clone(),
            reason,
        };
        let mut config = match read_file(config_dir)? {
            Some(text) => parse(&text).map_err(invalid)?,
            None => Self {
                providers: vec![Provider::Netbird],
                port: DEFAULT_REMOTE_PORT,
                status_deadline: DEFAULT_STATUS_DEADLINE,
                static_peers: None,
                from_file: false,
            },
        };
        if let Some(port) = port_override().map_err(invalid)? {
            config.port = port;
        }
        Ok(config)
    }

    /// Build the registry of the configured providers on the configured port.
    ///
    /// No provider is consulted: construction only wires the transports.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::OverlayRegistry`] when the registry rejects an
    /// entry, which a validated configuration never does.
    pub fn registry(&self) -> Result<OverlayRegistry, ClientError> {
        self.build_registry(true)
    }

    /// Build the registry without owner-configured static peers.
    ///
    /// This serves `host.resolve` to clients that cannot verify a host-id
    /// pin, so it leaves static peers out.
    ///
    /// # Errors
    ///
    /// Same as [`Self::registry`].
    pub fn registry_without_static_peers(&self) -> Result<OverlayRegistry, ClientError> {
        self.build_registry(false)
    }

    fn build_registry(&self, with_static_peers: bool) -> Result<OverlayRegistry, ClientError> {
        let entries = self
            .providers
            .iter()
            .map(|provider| {
                let peers = with_static_peers
                    .then(|| self.static_peers.clone())
                    .flatten();
                ConfiguredTransport::new(provider.transport(self.status_deadline, peers), self.port)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(OverlayRegistry::new(entries)?.with_status_budget(self.status_deadline))
    }
}

/// Build the production overlay registry from `config_dir`.
///
/// # Errors
///
/// Returns the errors of [`OverlayConfig::load_from`] and
/// [`OverlayConfig::registry`].
pub fn overlay_registry_from(config_dir: &Path) -> Result<OverlayRegistry, ClientError> {
    OverlayConfig::load_from(config_dir)?.registry()
}

/// Build the production overlay registry from the XDG config directory.
///
/// # Errors
///
/// Returns the errors of [`OverlayConfig::load`] and [`OverlayConfig::registry`].
pub fn default_overlay_registry() -> Result<OverlayRegistry, ClientError> {
    OverlayConfig::load()?.registry()
}

/// Probe every overlay of `registry` for its listener state.
///
/// Each provider gets `budget` on its own: one that has not answered when it
/// elapses is reported as unavailable, so a hung provider CLI neither outlasts a
/// doctor request nor hides the state of a healthy one.
pub async fn diagnose_overlays(
    registry: &OverlayRegistry,
    budget: Duration,
) -> Vec<OverlayDiagnostic> {
    join_all(registry.entries().iter().map(|entry| async move {
        match tokio::time::timeout(budget, entry.diagnostic()).await {
            Ok(diagnostic) => diagnostic,
            Err(_elapsed) => OverlayDiagnostic {
                overlay: entry.id().clone(),
                port: entry.port(),
                listener: None,
                error: Some(OverlayError::StateUnavailable {
                    overlay: entry.id().clone(),
                    detail: format!("the provider did not answer within {budget:?}"),
                }),
            },
        }
    }))
    .await
}

/// Resolve `<config_home>/pohunek`.
fn config_dir() -> Result<PathBuf, ClientError> {
    pohunek_paths::config_home()
        .map(|home| home.join(pohunek_paths::APP_DIR))
        .map_err(|error| ClientError::OverlayConfiguration {
            path: PathBuf::from(OVERLAY_CONFIG_FILE),
            reason: format!("the config directory cannot be resolved: {error}"),
        })
}

/// Read the file through a trusted directory; `None` when it or its directory
/// is absent.
fn read_file(config_dir: &Path) -> Result<Option<String>, ClientError> {
    let path = config_dir.join(OVERLAY_CONFIG_FILE);
    let invalid = |reason: String| ClientError::OverlayConfiguration {
        path: path.clone(),
        reason,
    };
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            return Ok(None)
        }
        Err(error) => {
            return Err(invalid(format!(
                "the file cannot be inspected: {}",
                error.kind()
            )))
        }
    }
    let bytes = match TrustedDir::open_absolute_owner_safe(config_dir, FORBIDDEN_DIRECTORY_BITS)
        .and_then(|dir| dir.read_file(OVERLAY_CONFIG_FILE, FILE_MODE, MAX_FILE_BYTES))
    {
        Ok(bytes) => bytes,
        Err(error) if error.io_kind() == Some(ErrorKind::NotFound) => return Ok(None),
        Err(error) => return Err(invalid(unsafe_reason(&error))),
    };
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_not_utf8| invalid("the file is not UTF-8".to_owned()))
}

/// Parse and validate the file text.
///
/// The provider count is checked before the ids, so a list of several entries
/// always reports the one-overlay limit.
fn parse(text: &str) -> Result<OverlayConfig, String> {
    // Parser messages can quote file values, so only the line survives.
    let raw: RawConfig = toml_edit::de::from_str(text).map_err(|error| {
        let line = error
            .span()
            .and_then(|span| text.get(..span.start))
            .map(|prefix| prefix.matches('\n').count() + 1);
        format!(
            "the file does not match the overlays schema (syntax, unknown key or wrong type){}",
            line.map(|line| format!(" at line {line}"))
                .unwrap_or_default()
        )
    })?;
    if raw.providers.is_empty() {
        return Err("providers must name exactly one overlay provider".to_owned());
    }
    if raw.providers.len() > 1 {
        return Err(format!(
            "providers names more than one overlay; one overlay per host is supported \
             (running several on one host is tracked in {MULTI_OVERLAY_ISSUE})"
        ));
    }
    let providers = raw
        .providers
        .iter()
        .map(|entry| match entry {
            Entry::Id(id) => Provider::from_id(id),
            Entry::Other(_) => None,
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            format!(
                "providers names an unknown overlay id (known ids: {})",
                KNOWN_PROVIDER_IDS.join(", ")
            )
        })?;
    let port = match raw.port {
        None => DEFAULT_REMOTE_PORT,
        Some(port) => u16::try_from(port)
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| "port must be a number in 1..=65535".to_owned())?,
    };
    let status_deadline = match raw.status_timeout_secs {
        None => DEFAULT_STATUS_DEADLINE,
        Some(secs) => u64::try_from(secs)
            .ok()
            .filter(|secs| (MIN_STATUS_DEADLINE_SECS..=MAX_STATUS_DEADLINE_SECS).contains(secs))
            .map(Duration::from_secs)
            .ok_or_else(|| {
                format!(
                    "status_timeout_secs must be a number in \
                     {MIN_STATUS_DEADLINE_SECS}..={MAX_STATUS_DEADLINE_SECS}"
                )
            })?,
    };
    let static_peers = match raw.netbird {
        None => None,
        Some(netbird) => Some(parse_static_peers(&providers, &netbird)?),
    };
    Ok(OverlayConfig {
        providers,
        port,
        status_deadline,
        static_peers,
        from_file: true,
    })
}

/// Validate the `[netbird]` table and build its static peers.
///
/// The first failure wins, in the order: provider, subnet, entry count,
/// per-entry fields, then uniqueness across entries. Messages name the
/// validated entry key but never a value from the file.
fn parse_static_peers(providers: &[Provider], netbird: &RawNetbird) -> Result<StaticPeers, String> {
    if providers != [Provider::Netbird] {
        return Err("the netbird table requires providers = [\"netbird\"]".to_owned());
    }
    let (network, prefix) = parse_subnet(&netbird.subnet)?;
    if netbird.static_peers.is_empty() || netbird.static_peers.len() > MAX_STATIC_PEERS {
        return Err(format!(
            "netbird.static_peers must hold 1..={MAX_STATIC_PEERS} entries"
        ));
    }
    let mut peers = Vec::with_capacity(netbird.static_peers.len());
    for (key, raw) in &netbird.static_peers {
        peers.push(parse_static_peer(key, raw, network, prefix)?);
    }
    check_uniqueness(&peers)?;
    Ok(StaticPeers::new(network, prefix, peers))
}

fn parse_static_peer(
    key: &str,
    raw: &RawStaticPeer,
    network: Ipv4Addr,
    prefix: u8,
) -> Result<StaticPeer, String> {
    if !is_dns_label(key) || key == crate::LOCAL_HOST {
        return Err("a netbird.static_peers entry name must be a lowercase DNS label (a-z, 0-9, '-', 1..=63 characters, no leading or trailing '-') other than 'local'".to_owned());
    }
    let address = raw.address.parse::<Ipv4Addr>().ok().filter(|address| {
        in_subnet(network, prefix, *address)
            && *address != network
            && *address != broadcast(network, prefix)
    });
    let Some(address) = address else {
        return Err(format!(
            "netbird.static_peers.{key}.address must be an IPv4 address inside netbird.subnet other than its network and broadcast address"
        ));
    };
    if !is_dns_name(&raw.fqdn) {
        return Err(format!(
            "netbird.static_peers.{key}.fqdn must be a lowercase DNS name with at least two labels and no trailing dot"
        ));
    }
    if protocol::HostId::parse(&raw.host_id).is_err() {
        return Err(format!(
            "netbird.static_peers.{key}.host_id must be a host id as pohunek host governance inspect prints it"
        ));
    }
    Ok(StaticPeer {
        name: key.to_owned(),
        fqdn: raw.fqdn.clone(),
        address,
        host_id: raw.host_id.clone(),
    })
}

fn check_uniqueness(peers: &[StaticPeer]) -> Result<(), String> {
    for (index, left) in peers.iter().enumerate() {
        for right in peers.iter().skip(index + 1) {
            let pair = |what: &str| {
                format!(
                    "netbird.static_peers.{} and netbird.static_peers.{} share {what}",
                    left.name, right.name
                )
            };
            if left.address == right.address {
                return Err(pair("an address"));
            }
            if left.fqdn == right.fqdn {
                return Err(pair("an fqdn"));
            }
            if left.host_id == right.host_id {
                return Err(pair("a host id"));
            }
            let left_names = selector_names(left);
            let right_names = selector_names(right);
            if left_names.iter().any(|name| right_names.contains(name)) {
                return Err(pair("a selector name"));
            }
        }
    }
    Ok(())
}

fn selector_names(peer: &StaticPeer) -> Vec<String> {
    let mut names = vec![peer.name.clone()];
    if let Some(short) = peer.fqdn.split('.').next() {
        if !short.is_empty() && short != peer.name {
            names.push(short.to_owned());
        }
    }
    names
}

/// Parse `a.b.c.d/p` and check it is an IPv4 network inside `100.64.0.0/10`.
fn parse_subnet(value: &str) -> Result<(Ipv4Addr, u8), String> {
    let invalid = || {
        "netbird.subnet must be an IPv4 network in CIDR form inside 100.64.0.0/10, prefix length 10..=30, with no host bits set".to_owned()
    };
    if value.chars().any(char::is_whitespace) {
        return Err(invalid());
    }
    let (address, prefix) = value.split_once('/').ok_or_else(invalid)?;
    let network = Ipv4Addr::from_str(address).map_err(|_invalid| invalid())?;
    let prefix = prefix.parse::<u8>().map_err(|_invalid| invalid())?;
    if !(10..=30).contains(&prefix) {
        return Err(invalid());
    }
    if !in_subnet(NETBIRD_CGNAT.0, NETBIRD_CGNAT.1, network) || network != network & mask(prefix) {
        return Err(invalid());
    }
    Ok((network, prefix))
}

fn mask(prefix: u8) -> Ipv4Addr {
    let bits = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    Ipv4Addr::from(bits)
}

fn in_subnet(network: Ipv4Addr, prefix: u8, address: Ipv4Addr) -> bool {
    let mask = u32::from(mask(prefix));
    u32::from(network) & mask == u32::from(address) & mask
}

fn broadcast(network: Ipv4Addr, prefix: u8) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(network) | !u32::from(mask(prefix)))
}

fn is_dns_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !value.starts_with('-')
        && !value.ends_with('-')
}

fn is_dns_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 253
        && !value.ends_with('.')
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'.'
        })
        && value.split('.').count() >= 2
        && value.split('.').all(is_dns_label)
}

/// Resolve [`REMOTE_PORT_ENV`]; `None` when it is unset.
///
/// A present-but-invalid value is a configuration error rather than a silent
/// fallback, so a typo fails loudly. A value that is not valid Unicode is the
/// same error, not a panicking lookup.
fn port_override() -> Result<Option<u16>, String> {
    match std::env::var(REMOTE_PORT_ENV) {
        Ok(raw) => {
            // Surrounding whitespace is tolerated, mirroring how operators
            // quote values in shell configuration.
            raw.trim()
                .parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .map(Some)
                .ok_or_else(|| {
                    format!("invalid {REMOTE_PORT_ENV}: expected a port number in 1..=65535")
                })
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!(
            "invalid {REMOTE_PORT_ENV}: value is not valid Unicode"
        )),
    }
}

/// Describes why the file could not be read safely, by cause only.
fn unsafe_reason(error: &FsError) -> String {
    match error {
        FsError::FileTooLarge { .. } => format!("the file exceeds {MAX_FILE_BYTES} bytes"),
        FsError::Io { .. } => format!(
            "the file or its directory cannot be read: {}",
            error.io_kind().unwrap_or(ErrorKind::Other)
        ),
        _ => format!(
            "the directory path must hold no symlink and be owned by you and not writable by \
             other accounts, and {OVERLAY_CONFIG_FILE} must be a regular file \
             owned by you with mode {FILE_MODE:o}"
        ),
    }
}
