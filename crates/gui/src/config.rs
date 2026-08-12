//! GUI configuration loading and validation.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use pohunek_gui_core::{ConnectionOptions, HostConfig};
use serde::Deserialize;
use thiserror::Error;

use crate::keyboard::{KeyMap, KeyMapError};

// 80x24 is the traditional terminal size expected by many CLI tools.
const DEFAULT_TERMINAL_COLS: u16 = 80;
const DEFAULT_TERMINAL_ROWS: u16 = 24;

// notify-send is the freedesktop notification CLI available on target Linux desktops.
const DEFAULT_NOTIFICATION_COMMAND: &str = "notify-send";

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
    pub(crate) keymap: KeyMap,
}

impl AppConfig {
    pub(crate) fn load() -> Result<Self, ConfigError> {
        let config_dir = config_dir()?;
        let path = config_dir.join("gui.toml");
        let raw = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
        let raw: RawConfig =
            toml::from_str(&raw).map_err(|source| ConfigError::Parse { path, source })?;
        let raw_gui = raw.gui.unwrap_or_default();
        Ok(Self {
            attach_command: raw.attach_command,
            pohunek_bin: raw.pohunek_bin,
            local_host: HostConfig::local("local", local_socket_path()?),
            connection_options: raw_gui.connection_options()?,
            terminal_size: raw_gui.terminal_size()?,
            notification_command: raw
                .notification_command
                .unwrap_or_else(|| DEFAULT_NOTIFICATION_COMMAND.to_owned()),
            keymap: keymap_from_raw_keybindings(&raw.keybindings)?,
        })
    }
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    attach_command: String,
    pohunek_bin: String,
    #[serde(default)]
    notification_command: Option<String>,
    #[serde(default)]
    gui: Option<RawGuiConfig>,
    #[serde(default)]
    keybindings: BTreeMap<String, String>,
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
    #[error("invalid keybindings: {source}")]
    Keybindings { source: KeyMapError },
}

fn keymap_from_raw_keybindings(raw: &BTreeMap<String, String>) -> Result<KeyMap, ConfigError> {
    KeyMap::from_config(raw).map_err(|source| ConfigError::Keybindings { source })
}

fn duration_millis(
    value: Option<u64>,
    field: &'static str,
    default: Duration,
) -> Result<Duration, ConfigError> {
    duration(value, field, default, Duration::from_millis)
}

fn duration_secs(
    value: Option<u64>,
    field: &'static str,
    default: Duration,
) -> Result<Duration, ConfigError> {
    duration(value, field, default, Duration::from_secs)
}

fn duration(
    value: Option<u64>,
    field: &'static str,
    default: Duration,
    convert: fn(u64) -> Duration,
) -> Result<Duration, ConfigError> {
    value.map_or(Ok(default), |value| {
        if value == 0 {
            Err(ConfigError::Invalid {
                field,
                message: "must be greater than zero".to_owned(),
            })
        } else {
            Ok(convert(value))
        }
    })
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
