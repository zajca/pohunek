//! GUI configuration: `gui.toml` loading, validation, and provider filter files.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use pohunek_gui_core::{providers, ConnectionOptions, HostConfig};
use serde::Deserialize;
use thiserror::Error;

use crate::keyboard::{KeyMap, KeyMapError};

// 80x24 is the traditional terminal size expected by many CLI tools.
const DEFAULT_TERMINAL_COLS: u16 = 80;

const DEFAULT_TERMINAL_ROWS: u16 = 24;

// notify-send is the freedesktop notification CLI available on target Linux desktops.
const DEFAULT_NOTIFICATION_COMMAND: &str = "notify-send";

// xdg-open is the freedesktop URL/file opener available on target Linux desktops;
// it dispatches to the user's configured default browser.
const DEFAULT_OPEN_URL_COMMAND: &str = "xdg-open";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalSize {
    pub(crate) cols: u16,
    pub(crate) rows: u16,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            cols: DEFAULT_TERMINAL_COLS,
            rows: DEFAULT_TERMINAL_ROWS,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AppConfig {
    pub(crate) attach_command: String,
    pub(crate) pohunek_bin: String,
    pub(crate) local_host: HostConfig,
    pub(crate) connection_options: ConnectionOptions,
    pub(crate) terminal_size: TerminalSize,
    pub(crate) notification_command: String,
    /// Command used to open a provider item's URL in the OS browser, spawned via
    /// argv (never a shell) so the URL cannot inject shell syntax.
    pub(crate) open_url_command: String,
    pub(crate) keymap: KeyMap,
    pub(crate) providers: ProviderAppConfig,
}

impl AppConfig {
    pub(crate) fn load() -> Result<Self, ConfigError> {
        let config_dir = config_dir()?;
        let path = config_dir.join("gui.toml");
        let raw = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
        let raw: RawConfig = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source,
        })?;
        let local_host = HostConfig::local("local", local_socket_path()?);
        let raw_gui = raw.gui.unwrap_or_default();
        let connection_options = raw_gui.connection_options()?;
        let terminal_size = raw_gui.terminal_size()?;
        let keymap = keymap_from_raw_keybindings(&raw.keybindings)?;
        Ok(Self {
            attach_command: raw.attach_command,
            pohunek_bin: raw.pohunek_bin,
            local_host,
            connection_options,
            terminal_size,
            notification_command: raw
                .notification_command
                .unwrap_or_else(|| DEFAULT_NOTIFICATION_COMMAND.to_owned()),
            open_url_command: raw
                .open_url_command
                .unwrap_or_else(|| DEFAULT_OPEN_URL_COMMAND.to_owned()),
            keymap,
            providers: raw.providers.unwrap_or_default().into_provider_config()?,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ProviderAppConfig {
    pub(crate) linear: Option<LinearAppConfig>,
    pub(crate) github: Option<GitHubAppConfig>,
    /// Host-layer (`gui.toml`) named filters, merged with the project layer and
    /// built-in defaults when the provider panels resolve their pickers.
    pub(crate) filters: providers::filters::ProviderFilterSet,
}

#[derive(Debug, Clone)]
pub(crate) struct LinearAppConfig {
    pub(crate) token_key: String,
    pub(crate) endpoint: String,
    pub(crate) token_lookup_timeout: Duration,
}

#[derive(Debug, Clone)]
pub(crate) struct GitHubAppConfig {
    pub(crate) gh_bin: PathBuf,
    pub(crate) timeout: Duration,
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    attach_command: String,
    pohunek_bin: String,
    #[serde(default)]
    notification_command: Option<String>,
    #[serde(default)]
    open_url_command: Option<String>,
    #[serde(default)]
    gui: Option<RawGuiConfig>,
    #[serde(default)]
    keybindings: BTreeMap<String, String>,
    #[serde(default)]
    providers: Option<RawProvidersConfig>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawGuiConfig {
    #[serde(default)]
    pub(crate) connect_timeout_ms: Option<u64>,
    #[serde(default)]
    pub(crate) request_timeout_ms: Option<u64>,
    #[serde(default)]
    pub(crate) reconcile_secs: Option<u64>,
    #[serde(default)]
    pub(crate) backoff_initial_ms: Option<u64>,
    #[serde(default)]
    pub(crate) backoff_max_ms: Option<u64>,
    #[serde(default)]
    pub(crate) terminal_cols: Option<u16>,
    #[serde(default)]
    pub(crate) terminal_rows: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
struct RawProvidersConfig {
    #[serde(default)]
    linear: Option<RawLinearProviderConfig>,
    #[serde(default)]
    github: Option<RawGitHubProviderConfig>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawLinearProviderConfig {
    pub(crate) token_key: String,
    pub(crate) endpoint: String,
    pub(crate) token_timeout_ms: u64,
    #[serde(default)]
    pub(crate) filters: Vec<RawLinearFilter>,
}

#[derive(Debug, Deserialize)]
struct RawGitHubProviderConfig {
    gh_bin: PathBuf,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    filters: Vec<RawGitHubFilter>,
}

/// One `[providers.github]` (or in-repo) pull request filter as written in TOML.
#[derive(Debug, Deserialize)]
struct RawGitHubFilter {
    name: String,
    #[serde(default)]
    search: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

/// One `[providers.linear]` (or in-repo) issue filter as written in TOML.
#[derive(Debug, Deserialize)]
pub(crate) struct RawLinearFilter {
    name: String,
    /// Raw Linear `IssueFilter` as a TOML table, converted to JSON at load time.
    filter: toml::Value,
}

/// In-repo `<repo_root>/.pohunek/providers.toml` filter layer.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawProjectFilters {
    #[serde(default)]
    github: Vec<RawGitHubFilter>,
    #[serde(default)]
    linear: Vec<RawLinearFilter>,
}

impl RawGitHubFilter {
    fn into_filter(self) -> Result<providers::filters::GitHubFilter, ConfigError> {
        let name = non_empty_config_value(self.name, "providers.github.filters[].name")?;
        let state = match self.state {
            Some(state) => providers::filters::GitHubPrState::parse(&state).map_err(|source| {
                ConfigError::ProviderFilter {
                    message: source.to_string(),
                }
            })?,
            None => providers::filters::GitHubPrState::default(),
        };
        Ok(providers::filters::GitHubFilter::new(
            name,
            self.search.unwrap_or_default(),
            state,
        ))
    }
}

impl RawLinearFilter {
    fn into_filter(self) -> Result<providers::filters::LinearFilter, ConfigError> {
        let name = non_empty_config_value(self.name, "providers.linear.filters[].name")?;
        let filter =
            serde_json::to_value(self.filter).map_err(|source| ConfigError::ProviderFilter {
                message: format!("invalid Linear filter `{name}`: {source}"),
            })?;
        Ok(providers::filters::LinearFilter::new(name, filter))
    }
}

impl RawProjectFilters {
    pub(crate) fn into_filter_set(
        self,
    ) -> Result<providers::filters::ProviderFilterSet, ConfigError> {
        Ok(providers::filters::ProviderFilterSet {
            github: self
                .github
                .into_iter()
                .map(RawGitHubFilter::into_filter)
                .collect::<Result<_, _>>()?,
            linear: self
                .linear
                .into_iter()
                .map(RawLinearFilter::into_filter)
                .collect::<Result<_, _>>()?,
        })
    }
}

impl RawProvidersConfig {
    fn into_provider_config(self) -> Result<ProviderAppConfig, ConfigError> {
        let mut filters = providers::filters::ProviderFilterSet::default();
        let linear = match self.linear {
            Some(raw) => {
                let (config, linear_filters) = raw.into_app_config()?;
                filters.linear = linear_filters;
                Some(config)
            }
            None => None,
        };
        let github = match self.github {
            Some(raw) => {
                let (config, github_filters) = raw.into_app_config()?;
                filters.github = github_filters;
                Some(config)
            }
            None => None,
        };
        Ok(ProviderAppConfig {
            linear,
            github,
            filters,
        })
    }
}

impl RawLinearProviderConfig {
    pub(crate) fn into_app_config(
        self,
    ) -> Result<(LinearAppConfig, Vec<providers::filters::LinearFilter>), ConfigError> {
        let filters = self
            .filters
            .into_iter()
            .map(RawLinearFilter::into_filter)
            .collect::<Result<Vec<_>, _>>()?;
        let config = LinearAppConfig {
            token_key: non_empty_config_value(self.token_key, "providers.linear.token_key")?,
            endpoint: validate_http_endpoint(self.endpoint, "providers.linear.endpoint")?,
            token_lookup_timeout: required_duration_millis(
                self.token_timeout_ms,
                "providers.linear.token_timeout_ms",
            )?,
        };
        Ok((config, filters))
    }
}

impl RawGitHubProviderConfig {
    fn into_app_config(
        self,
    ) -> Result<(GitHubAppConfig, Vec<providers::filters::GitHubFilter>), ConfigError> {
        let filters = self
            .filters
            .into_iter()
            .map(RawGitHubFilter::into_filter)
            .collect::<Result<Vec<_>, _>>()?;
        let config = GitHubAppConfig {
            gh_bin: non_empty_config_path(self.gh_bin, "providers.github.gh_bin")?,
            timeout: duration_millis(
                self.timeout_ms,
                "providers.github.timeout_ms",
                Duration::from_secs(20),
            )?,
        };
        Ok((config, filters))
    }
}

impl RawGuiConfig {
    fn connection_options(&self) -> Result<ConnectionOptions, ConfigError> {
        let defaults = ConnectionOptions::default();
        Ok(ConnectionOptions {
            connect_timeout: duration_millis(
                self.connect_timeout_ms,
                "gui.connect_timeout_ms",
                defaults.connect_timeout,
            )?,
            request_timeout: duration_millis(
                self.request_timeout_ms,
                "gui.request_timeout_ms",
                defaults.request_timeout,
            )?,
            reconcile_interval: duration_secs(
                self.reconcile_secs,
                "gui.reconcile_secs",
                defaults.reconcile_interval,
            )?,
            backoff_initial: duration_millis(
                self.backoff_initial_ms,
                "gui.backoff_initial_ms",
                defaults.backoff_initial,
            )?,
            backoff_max: duration_millis(
                self.backoff_max_ms,
                "gui.backoff_max_ms",
                defaults.backoff_max,
            )?,
        })
    }

    pub(crate) fn terminal_size(&self) -> Result<TerminalSize, ConfigError> {
        Ok(TerminalSize {
            cols: terminal_dimension(
                self.terminal_cols,
                "gui.terminal_cols",
                DEFAULT_TERMINAL_COLS,
            )?,
            rows: terminal_dimension(
                self.terminal_rows,
                "gui.terminal_rows",
                DEFAULT_TERMINAL_ROWS,
            )?,
        })
    }
}

#[derive(Debug, Error)]
pub(crate) enum ConfigError {
    #[error("missing environment variable `{var}`")]
    MissingEnv { var: String },
    #[error("failed to read `{}`: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse `{}`: {source}", path.display())]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("invalid `{field}`: {message}")]
    Invalid {
        field: &'static str,
        message: String,
    },
    #[error("invalid provider filter: {message}")]
    ProviderFilter { message: String },
    #[error("invalid keybindings: {source}")]
    Keybindings { source: KeyMapError },
}

fn keymap_from_raw_keybindings(raw: &BTreeMap<String, String>) -> Result<KeyMap, ConfigError> {
    KeyMap::from_config(raw).map_err(|source| ConfigError::Keybindings { source })
}

fn non_empty_config_value(value: String, field: &'static str) -> Result<String, ConfigError> {
    if value.trim().is_empty() {
        Err(ConfigError::Invalid {
            field,
            message: "must not be empty".to_owned(),
        })
    } else {
        Ok(value)
    }
}

pub(crate) fn non_empty_config_path(
    value: PathBuf,
    field: &'static str,
) -> Result<PathBuf, ConfigError> {
    if value.as_os_str().is_empty() {
        Err(ConfigError::Invalid {
            field,
            message: "must not be empty".to_owned(),
        })
    } else if value.components().count() > 1 && !value.exists() {
        Err(ConfigError::Invalid {
            field,
            message: "path does not exist".to_owned(),
        })
    } else {
        Ok(value)
    }
}

pub(crate) fn validate_http_endpoint(
    value: String,
    field: &'static str,
) -> Result<String, ConfigError> {
    let value = non_empty_config_value(value, field)?;
    let Some(rest) = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
    else {
        return Err(ConfigError::Invalid {
            field,
            message: "must start with http:// or https://".to_owned(),
        });
    };
    if rest.split('/').next().is_none_or(str::is_empty) {
        return Err(ConfigError::Invalid {
            field,
            message: "must include a host".to_owned(),
        });
    }
    Ok(value)
}

fn duration_millis(
    value: Option<u64>,
    field: &'static str,
    default: Duration,
) -> Result<Duration, ConfigError> {
    value.map_or(Ok(default), |millis| {
        if millis == 0 {
            Err(ConfigError::Invalid {
                field,
                message: "must be greater than zero".to_owned(),
            })
        } else {
            Ok(Duration::from_millis(millis))
        }
    })
}

fn required_duration_millis(value: u64, field: &'static str) -> Result<Duration, ConfigError> {
    if value == 0 {
        Err(ConfigError::Invalid {
            field,
            message: "must be greater than zero".to_owned(),
        })
    } else {
        Ok(Duration::from_millis(value))
    }
}

fn terminal_dimension(
    value: Option<u16>,
    field: &'static str,
    default: u16,
) -> Result<u16, ConfigError> {
    value.map_or(Ok(default), |dimension| {
        if dimension == 0 {
            Err(ConfigError::Invalid {
                field,
                message: "must be greater than zero".to_owned(),
            })
        } else {
            Ok(dimension)
        }
    })
}

fn duration_secs(
    value: Option<u64>,
    field: &'static str,
    default: Duration,
) -> Result<Duration, ConfigError> {
    value.map_or(Ok(default), |secs| {
        if secs == 0 {
            Err(ConfigError::Invalid {
                field,
                message: "must be greater than zero".to_owned(),
            })
        } else {
            Ok(Duration::from_secs(secs))
        }
    })
}

fn local_socket_path() -> Result<PathBuf, ConfigError> {
    pohunek_paths::socket_path().map_err(config_path_error)
}

fn config_dir() -> Result<PathBuf, ConfigError> {
    pohunek_paths::config_home()
        .map(|home| home.join(pohunek_paths::APP_DIR))
        .map_err(config_path_error)
}

fn config_path_error(err: pohunek_paths::PathError) -> ConfigError {
    match err {
        pohunek_paths::PathError::MissingEnv { var } => ConfigError::MissingEnv { var },
    }
}

#[cfg(test)]
mod tests {
    use iced::keyboard::Modifiers;

    use super::*;
    use crate::keyboard::{KeyAction, KeyChord, KeyContext};

    #[test]
    fn keybindings_table_builds_config_keymap() {
        let raw: RawConfig = toml::from_str(
            r#"
attach_command = "foot -- {bin} attach {host} {id}"
pohunek_bin = "pohunek"

[keybindings]
open_inbox = "ctrl+i"
"#,
        )
        .expect("raw config");

        let keymap = keymap_from_raw_keybindings(&raw.keybindings).expect("keymap");

        assert_eq!(
            keymap.action_for(
                KeyContext::Global,
                &KeyChord::character("i").with_modifiers(Modifiers::CTRL)
            ),
            Some(KeyAction::OpenInbox)
        );
    }
}
