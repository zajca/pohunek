//! GUI configuration loading and validation.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use pohunek_gui_core::{
    validate_attach_argv_template, validate_attach_shell_template, AttachTemplateError,
    ConnectionOptions, HostConfig,
};
use pohunek_platform::shell_env::{LOGIN_SHELL_OUTPUT, LOGIN_SHELL_TIMEOUT};
use serde::Deserialize;
use thiserror::Error;

use crate::bin_resolver::{BinResolver, LoginShellSettings};
use crate::keyboard::{KeyMap, KeyMapError};
use crate::notify::{
    CommandResolution, NotificationBackend, Notifier, DEFAULT_NOTIFY_COMMAND, SYSTEM_OSASCRIPT,
};
use crate::terminal::AttachTerminal;

// 80x24 is the traditional terminal size expected by many CLI tools.
const DEFAULT_TERMINAL_COLS: u16 = 80;
const DEFAULT_TERMINAL_ROWS: u16 = 24;

// `open` hands the script to LaunchServices and returns as soon as the request
// is accepted; five seconds tolerates a cold Terminal.app launch while a wedged
// LaunchServices does not block the attach status forever.
const DEFAULT_OPEN_TIMEOUT: Duration = Duration::from_secs(5);

// A launched terminal that dies within this window (a dead template, a missing
// program) is reported as a failure; a healthy terminal keeps running past it.
// One second covers process start and an immediate usage error.
const DEFAULT_ATTACH_OBSERVE: Duration = Duration::from_secs(1);

// Terminal.app runs an attach script within moments of `open`; a script older
// than an hour was never run. The age only bounds how long host and session id
// stay on disk after a launch Terminal never completed.
const DEFAULT_ATTACH_SCRIPT_MAX_AGE: Duration = Duration::from_secs(3600);

// A notification backend answers within a moment; five seconds tolerates a
// cold osascript start while a wedged backend cannot pin a blocking-pool thread.
const DEFAULT_NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(5);

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

/// How the configured `attach_command` template is executed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AttachCommandMode {
    /// The rendered template runs through `sh -c` with shell-escaped values.
    #[default]
    Shell,
    /// The template is split into words and executed without a shell.
    Argv,
}

/// The one way the GUI opens a session in a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AttachSelection {
    /// A user-supplied command template.
    Command {
        template: String,
        mode: AttachCommandMode,
    },
    /// A stock terminal application driven by the GUI itself.
    Terminal(AttachTerminal),
}

/// Timeouts and bounds of attach launching, all overridable under `[gui]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LaunchSettings {
    pub(crate) open_timeout: Duration,
    pub(crate) login_shell_timeout: Duration,
    pub(crate) login_shell_max_output_bytes: usize,
    pub(crate) notification_timeout: Duration,
    pub(crate) attach_observe: Duration,
    pub(crate) attach_script_max_age: Duration,
}

#[derive(Debug, Clone)]
pub(crate) struct AppConfig {
    pub(crate) attach: AttachSelection,
    pub(crate) pohunek_bin: String,
    pub(crate) launch: LaunchSettings,
    /// Shared so the resolved search path survives across attach actions.
    pub(crate) bin_resolver: Arc<BinResolver>,
    pub(crate) local_host: HostConfig,
    pub(crate) connection_options: ConnectionOptions,
    pub(crate) terminal_size: TerminalSize,
    pub(crate) notification: Notifier,
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
        let raw_gui = raw.gui.clone().unwrap_or_default();
        validate_text_field("pohunek_bin", &raw.pohunek_bin)?;
        if let Some(command) = &raw.notification_command {
            validate_text_field("notification_command", command)?;
        }
        let attach = attach_selection(&raw, cfg!(target_os = "macos"))?;
        let launch = raw_gui.launch_settings()?;
        let login_shell = LoginShellSettings {
            timeout: launch.login_shell_timeout,
            max_output_bytes: launch.login_shell_max_output_bytes,
        };
        let bin_resolver = Arc::new(BinResolver::for_host(&raw.pohunek_bin, login_shell));
        Ok(Self {
            attach,
            pohunek_bin: raw.pohunek_bin,
            launch,
            bin_resolver: Arc::clone(&bin_resolver),
            local_host: HostConfig::local("local", local_socket_path()?),
            connection_options: raw_gui.connection_options()?,
            terminal_size: raw_gui.terminal_size()?,
            notification: notifier(
                raw.notification_command.as_deref(),
                cfg!(target_os = "macos"),
                &launch,
                &bin_resolver,
            ),
            keymap: keymap_from_raw_keybindings(&raw.keybindings)?,
        })
    }
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    attach_command: Option<String>,
    #[serde(default)]
    attach_command_mode: Option<AttachCommandMode>,
    #[serde(default)]
    attach_terminal: Option<AttachTerminal>,
    pohunek_bin: String,
    #[serde(default)]
    notification_command: Option<String>,
    #[serde(default)]
    gui: Option<RawGuiConfig>,
    #[serde(default)]
    keybindings: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
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
    #[serde(default)]
    pub(crate) open_timeout_ms: Option<u64>,
    #[serde(default)]
    pub(crate) login_shell_timeout_ms: Option<u64>,
    #[serde(default)]
    pub(crate) login_shell_max_output_bytes: Option<usize>,
    #[serde(default)]
    pub(crate) notification_timeout_ms: Option<u64>,
    #[serde(default)]
    pub(crate) attach_observe_ms: Option<u64>,
    #[serde(default)]
    pub(crate) attach_script_max_age_secs: Option<u64>,
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
            origin_source: defaults.origin_source,
        })
    }

    pub(crate) fn launch_settings(&self) -> Result<LaunchSettings, ConfigError> {
        let max_output = self
            .login_shell_max_output_bytes
            .unwrap_or(LOGIN_SHELL_OUTPUT);
        if max_output == 0 {
            return Err(ConfigError::Invalid {
                field: "gui.login_shell_max_output_bytes",
                message: "must be greater than zero".to_owned(),
            });
        }
        Ok(LaunchSettings {
            open_timeout: duration_millis(
                self.open_timeout_ms,
                "gui.open_timeout_ms",
                DEFAULT_OPEN_TIMEOUT,
            )?,
            login_shell_timeout: duration_millis(
                self.login_shell_timeout_ms,
                "gui.login_shell_timeout_ms",
                LOGIN_SHELL_TIMEOUT,
            )?,
            login_shell_max_output_bytes: max_output,
            notification_timeout: duration_millis(
                self.notification_timeout_ms,
                "gui.notification_timeout_ms",
                DEFAULT_NOTIFICATION_TIMEOUT,
            )?,
            attach_observe: duration_millis(
                self.attach_observe_ms,
                "gui.attach_observe_ms",
                DEFAULT_ATTACH_OBSERVE,
            )?,
            attach_script_max_age: duration_secs(
                self.attach_script_max_age_secs,
                "gui.attach_script_max_age_secs",
                DEFAULT_ATTACH_SCRIPT_MAX_AGE,
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
    #[error("invalid application path configuration: {source}")]
    Paths { source: pohunek_paths::PathError },
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
    #[error(
        "set exactly one of `attach_command` or `attach_terminal` in gui.toml; neither is set"
    )]
    AttachMissing,
    #[error("`attach_command` and `attach_terminal` are mutually exclusive; remove one")]
    AttachConflict,
    #[error("`attach_command_mode` only applies together with `attach_command`")]
    AttachModeWithoutCommand,
    #[error("invalid `attach_command`: {source}")]
    AttachTemplate { source: AttachTemplateError },
    #[error("`attach_terminal = \"{terminal}\"` is only supported on macOS")]
    AttachTerminalUnsupported { terminal: &'static str },
    #[error("invalid keybindings: {source}")]
    Keybindings { source: KeyMapError },
}

/// Chooses the notification backend.
///
/// An explicit `notification_command` wins on every platform. Otherwise macOS
/// uses `osascript` and other hosts use `notify-send`. `darwin` is a parameter
/// so both defaults are testable on any host.
fn notifier(
    command: Option<&str>,
    darwin: bool,
    launch: &LaunchSettings,
    resolver: &Arc<BinResolver>,
) -> Notifier {
    let backend = match command {
        None if darwin => NotificationBackend::Osascript {
            executable: SYSTEM_OSASCRIPT.into(),
        },
        configured => NotificationBackend::Command {
            resolution: Arc::new(CommandResolution::new(
                Arc::clone(resolver),
                configured.unwrap_or(DEFAULT_NOTIFY_COMMAND),
            )),
        },
    };
    Notifier {
        backend,
        timeout: launch.notification_timeout,
    }
}

/// Rejects a program or template value that is blank or holds a NUL byte, so
/// the mistake fails at load instead of on every attach or notification.
fn validate_text_field(field: &'static str, value: &str) -> Result<(), ConfigError> {
    if value.trim().is_empty() {
        return Err(ConfigError::Invalid {
            field,
            message: "must not be empty".to_owned(),
        });
    }
    if value.contains('\0') {
        return Err(ConfigError::Invalid {
            field,
            message: "must not contain a NUL byte".to_owned(),
        });
    }
    Ok(())
}

/// Picks the single attach mechanism the configuration selects.
///
/// `darwin` is a parameter so the macOS-only branch is testable on any host.
fn attach_selection(raw: &RawConfig, darwin: bool) -> Result<AttachSelection, ConfigError> {
    match (&raw.attach_command, raw.attach_terminal) {
        (Some(_), Some(_)) => Err(ConfigError::AttachConflict),
        (None, None) => Err(ConfigError::AttachMissing),
        (Some(template), None) => {
            validate_text_field("attach_command", template)?;
            let mode = raw.attach_command_mode.unwrap_or_default();
            match mode {
                AttachCommandMode::Shell => validate_attach_shell_template(template),
                AttachCommandMode::Argv => validate_attach_argv_template(template),
            }
            .map_err(|source| ConfigError::AttachTemplate { source })?;
            Ok(AttachSelection::Command {
                template: template.clone(),
                mode,
            })
        }
        (None, Some(terminal)) => {
            if raw.attach_command_mode.is_some() {
                return Err(ConfigError::AttachModeWithoutCommand);
            }
            if terminal.requires_darwin() && !darwin {
                return Err(ConfigError::AttachTerminalUnsupported {
                    terminal: terminal.config_name(),
                });
            }
            Ok(AttachSelection::Terminal(terminal))
        }
    }
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
        source => ConfigError::Paths { source },
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

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

    fn raw(toml_text: &str) -> RawConfig {
        toml::from_str(toml_text).expect("raw config")
    }

    #[test]
    fn attach_command_alone_selects_the_shell_template() {
        let raw = raw("attach_command = \"foot -- {bin} attach {id}\"\npohunek_bin = \"pohunek\"");

        assert_eq!(
            attach_selection(&raw, false).expect("selection"),
            AttachSelection::Command {
                template: "foot -- {bin} attach {id}".to_owned(),
                mode: AttachCommandMode::Shell,
            }
        );
    }

    #[test]
    fn an_unsafe_attach_template_fails_at_config_load() {
        let shell = raw("attach_command = \"echo # {host}\"\npohunek_bin = \"pohunek\"");
        assert!(matches!(
            attach_selection(&shell, false),
            Err(ConfigError::AttachTemplate {
                source: AttachTemplateError::UnsafePlaceholderContext {
                    context: "a comment"
                }
            })
        ));
        let argv = raw(
            "attach_command = \"kitty '\"\nattach_command_mode = \"argv\"\npohunek_bin = \"pohunek\"",
        );
        assert!(matches!(
            attach_selection(&argv, false),
            Err(ConfigError::AttachTemplate {
                source: AttachTemplateError::UnterminatedQuote
            })
        ));
    }

    #[test]
    fn attach_command_mode_argv_is_selectable() {
        let raw = raw(
            "attach_command = \"kitty -e {bin} attach {id}\"\nattach_command_mode = \"argv\"\npohunek_bin = \"pohunek\"",
        );

        assert_eq!(
            attach_selection(&raw, false).expect("selection"),
            AttachSelection::Command {
                template: "kitty -e {bin} attach {id}".to_owned(),
                mode: AttachCommandMode::Argv,
            }
        );
    }

    #[test]
    fn unknown_attach_command_mode_is_a_parse_error() {
        let err = toml::from_str::<RawConfig>(
            "attach_command = \"x\"\nattach_command_mode = \"powershell\"\npohunek_bin = \"p\"",
        )
        .expect_err("unknown mode");
        assert!(err.to_string().contains("powershell"));
    }

    #[test]
    fn attach_terminal_alone_selects_the_stock_terminal_on_macos() {
        let raw = raw("attach_terminal = \"terminal-app\"\npohunek_bin = \"pohunek\"");

        assert_eq!(
            attach_selection(&raw, true).expect("selection"),
            AttachSelection::Terminal(AttachTerminal::TerminalApp)
        );
    }

    #[test]
    fn attach_terminal_off_macos_is_a_typed_error() {
        let raw = raw("attach_terminal = \"terminal-app\"\npohunek_bin = \"pohunek\"");

        let err = attach_selection(&raw, false).expect_err("unsupported");

        assert!(matches!(
            err,
            ConfigError::AttachTerminalUnsupported {
                terminal: "terminal-app"
            }
        ));
        assert!(err.to_string().contains("only supported on macOS"));
    }

    #[test]
    fn attach_command_and_attach_terminal_are_mutually_exclusive() {
        let raw = raw(
            "attach_command = \"x\"\nattach_terminal = \"terminal-app\"\npohunek_bin = \"pohunek\"",
        );

        assert!(matches!(
            attach_selection(&raw, true),
            Err(ConfigError::AttachConflict)
        ));
    }

    #[test]
    fn neither_attach_key_fails_fast() {
        let raw = raw("pohunek_bin = \"pohunek\"");

        let err = attach_selection(&raw, true).expect_err("missing");

        assert!(matches!(err, ConfigError::AttachMissing));
        assert!(err.to_string().contains("attach_command"));
        assert!(err.to_string().contains("attach_terminal"));
    }

    #[test]
    fn attach_command_mode_requires_attach_command() {
        let raw = raw(
            "attach_terminal = \"terminal-app\"\nattach_command_mode = \"argv\"\npohunek_bin = \"pohunek\"",
        );

        assert!(matches!(
            attach_selection(&raw, true),
            Err(ConfigError::AttachModeWithoutCommand)
        ));
    }

    #[test]
    fn blank_or_nul_text_fields_fail_at_load() {
        for value in ["", "   ", "a\0b"] {
            let err = validate_text_field("pohunek_bin", value).expect_err("invalid");
            assert!(
                matches!(
                    &err,
                    ConfigError::Invalid {
                        field: "pohunek_bin",
                        ..
                    }
                ),
                "{err}"
            );
        }
        validate_text_field("pohunek_bin", "/opt/pohunek").expect("valid");
    }

    #[test]
    fn a_nul_in_the_toml_string_is_caught_by_the_field_check() {
        let raw: RawConfig = toml::from_str(
            "attach_terminal = \"terminal-app\"\npohunek_bin = \"a\\u0000b\"\nnotification_command = \"n\\u0000\"",
        )
        .expect("raw config");
        validate_text_field("pohunek_bin", &raw.pohunek_bin).expect_err("NUL in pohunek_bin");
        let command = raw.notification_command.expect("command");
        validate_text_field("notification_command", &command)
            .expect_err("NUL in notification_command");
    }

    #[test]
    fn a_blank_attach_command_is_refused() {
        for template in ["", "  "] {
            let raw = RawConfig {
                attach_command: Some(template.to_owned()),
                ..raw("attach_command = \"x\"\npohunek_bin = \"p\"")
            };
            let err = attach_selection(&raw, true).expect_err("blank");
            assert!(
                matches!(
                    &err,
                    ConfigError::Invalid {
                        field: "attach_command",
                        ..
                    }
                ),
                "{err}"
            );
        }
        let raw = RawConfig {
            attach_command: Some("a\0b".to_owned()),
            ..raw("attach_command = \"x\"\npohunek_bin = \"p\"")
        };
        attach_selection(&raw, true).expect_err("NUL");
    }

    #[test]
    fn pohunek_bin_stays_required() {
        let err = toml::from_str::<RawConfig>("attach_command = \"x\"").expect_err("missing bin");
        assert!(err.to_string().contains("pohunek_bin"));
    }

    #[test]
    fn launch_settings_use_documented_defaults_and_accept_overrides() {
        let defaults = RawGuiConfig::default().launch_settings().expect("defaults");
        assert_eq!(defaults.open_timeout, DEFAULT_OPEN_TIMEOUT);
        assert_eq!(defaults.login_shell_timeout, LOGIN_SHELL_TIMEOUT);
        assert_eq!(defaults.login_shell_max_output_bytes, LOGIN_SHELL_OUTPUT);

        let custom = RawGuiConfig {
            open_timeout_ms: Some(250),
            login_shell_timeout_ms: Some(2_000),
            login_shell_max_output_bytes: Some(1_024),
            ..RawGuiConfig::default()
        }
        .launch_settings()
        .expect("custom");
        assert_eq!(custom.open_timeout, Duration::from_millis(250));
        assert_eq!(custom.login_shell_timeout, Duration::from_secs(2));
        assert_eq!(custom.login_shell_max_output_bytes, 1_024);
        assert_eq!(defaults.notification_timeout, DEFAULT_NOTIFICATION_TIMEOUT);
        assert_eq!(defaults.attach_observe, DEFAULT_ATTACH_OBSERVE);
        assert_eq!(
            defaults.attach_script_max_age,
            DEFAULT_ATTACH_SCRIPT_MAX_AGE
        );
    }

    #[test]
    fn zero_launch_settings_are_rejected() {
        for raw in [
            RawGuiConfig {
                open_timeout_ms: Some(0),
                ..RawGuiConfig::default()
            },
            RawGuiConfig {
                login_shell_timeout_ms: Some(0),
                ..RawGuiConfig::default()
            },
            RawGuiConfig {
                login_shell_max_output_bytes: Some(0),
                ..RawGuiConfig::default()
            },
            RawGuiConfig {
                notification_timeout_ms: Some(0),
                ..RawGuiConfig::default()
            },
            RawGuiConfig {
                attach_observe_ms: Some(0),
                ..RawGuiConfig::default()
            },
            RawGuiConfig {
                attach_script_max_age_secs: Some(0),
                ..RawGuiConfig::default()
            },
        ] {
            let err = raw.launch_settings().expect_err("zero");
            assert!(err.to_string().contains("must be greater than zero"));
        }
    }

    fn test_settings() -> (LaunchSettings, Arc<BinResolver>) {
        let launch = RawGuiConfig::default().launch_settings().expect("defaults");
        let resolver = Arc::new(BinResolver::with_discovery("pohunek", || {
            Err(crate::bin_resolver::BinError::SearchPath(
                "unused".to_owned(),
            ))
        }));
        (launch, resolver)
    }

    #[test]
    fn darwin_defaults_to_osascript_and_other_hosts_to_notify_send() {
        let (launch, resolver) = test_settings();

        let darwin = notifier(None, true, &launch, &resolver);
        assert!(matches!(
            darwin.backend,
            NotificationBackend::Osascript { ref executable } if executable == Path::new(SYSTEM_OSASCRIPT)
        ));
        assert_eq!(darwin.timeout, DEFAULT_NOTIFICATION_TIMEOUT);

        let linux = notifier(None, false, &launch, &resolver);
        assert!(matches!(
            linux.backend,
            NotificationBackend::Command { ref resolution } if format!("{resolution:?}").contains(DEFAULT_NOTIFY_COMMAND)
        ));
    }

    #[test]
    fn an_explicit_notification_command_wins_on_both_platforms() {
        let (launch, resolver) = test_settings();

        for darwin in [true, false] {
            let configured = notifier(Some("my-notify"), darwin, &launch, &resolver);
            assert!(matches!(
                configured.backend,
                NotificationBackend::Command { ref resolution } if format!("{resolution:?}").contains("my-notify")
            ));
        }
    }
}
