//! Overlay provider configuration, read from `<config>/pohunek/overlays.toml`.
//!
//! The daemon, the CLI and external Rust clients share this loader, so one
//! owner-private file decides which overlay a host works over and which TCP
//! port the remote control listener uses. The file is optional: when it or its
//! directory is absent, the host works over `NetBird` on [`DEFAULT_REMOTE_PORT`].
//!
//! Every component of the config directory path is opened without following
//! symlinks and must be owned by the user (or by root and not writable) and not
//! be writable by other accounts, so a symlinked or group-writable `~/.config`
//! is refused even when the file is absent. The directory, when it exists,
//! must be one no other account can write,
//! whether or not the file is there, because another account could otherwise
//! plant the file. When the file exists it must be a regular file owned by the
//! user with mode `0600`. Parsing is strict: unknown keys, wrong types and
//! out-of-range values fail with [`ClientError::OverlayConfiguration`] before
//! any provider is consulted. Error messages carry the file path and, at most,
//! a line number, never a value from the file.
//!
//! `POHUNEK_REMOTE_PORT`, when set, overrides the file's `port`. The file is
//! validated either way.

// Rust guideline compliant 2026-10-23

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use netbird::{NetbirdTransport, NETBIRD_OVERLAY_ID};
use overlay::{ConfiguredTransport, OverlayRegistry, OverlayTransport};
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
const KNOWN_PROVIDER_IDS: [&str; 1] = [NETBIRD_OVERLAY_ID];

/// A provider the loader knows how to construct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provider {
    Netbird,
}

impl Provider {
    fn from_id(id: &str) -> Option<Self> {
        match id {
            NETBIRD_OVERLAY_ID => Some(Self::Netbird),
            _ => None,
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Netbird => NETBIRD_OVERLAY_ID,
        }
    }

    fn transport(self) -> Arc<dyn OverlayTransport> {
        match self {
            Self::Netbird => Arc::new(NetbirdTransport::new()),
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
}

/// The overlay providers a host works over and the shared remote port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayConfig {
    providers: Vec<Provider>,
    port: u16,
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

    /// TCP port of the remote control listener on every configured overlay.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
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
        let entries = self
            .providers
            .iter()
            .map(|provider| ConfiguredTransport::new(provider.transport(), self.port))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(OverlayRegistry::new(entries)?)
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
    Ok(OverlayConfig { providers, port })
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
            "the directory must be owned by you and not writable by other accounts, even \
             without a {OVERLAY_CONFIG_FILE}, and {OVERLAY_CONFIG_FILE} must be a regular file \
             owned by you with mode {FILE_MODE:o}"
        ),
    }
}
