//! Persisted UI layout, selection, tree-node, and detail-tab view-model types.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use protocol::SessionId;

use crate::{
    HostId, DEFAULT_LEFT_PANE_WIDTH, DEFAULT_WINDOW_HEIGHT, DEFAULT_WINDOW_WIDTH, UI_STATE_FILE,
};

/// Active detail selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Selection {
    Host {
        host_id: HostId,
    },
    Project {
        host_id: HostId,
        project_id: String,
    },
    Session {
        host_id: HostId,
        session_id: SessionId,
    },
}

/// Persisted expanded workspace tree node.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TreeNodeId {
    Host { host_id: HostId },
    Project { host_id: HostId, project_id: String },
}

impl TreeNodeId {
    /// Construct a host node id.
    #[must_use]
    pub fn host(host_id: HostId) -> Self {
        Self::Host { host_id }
    }

    /// Construct a project node id.
    #[must_use]
    pub fn project(host_id: HostId, project_id: impl Into<String>) -> Self {
        Self::Project {
            host_id,
            project_id: project_id.into(),
        }
    }
}

/// Persisted window dimensions.
///
/// These remain `u32` for compatibility with existing TOML state; the Iced
/// shell clamps values to the platform window range when converting to/from
/// floating-point pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowSize {
    pub width: u32,
    pub height: u32,
}

/// Persisted UI layout and selection state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiState {
    pub left_pane_width: u16,
    pub window_size: WindowSize,
    pub expanded_nodes: BTreeSet<TreeNodeId>,
    pub selection: Option<Selection>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            left_pane_width: DEFAULT_LEFT_PANE_WIDTH,
            window_size: WindowSize {
                width: DEFAULT_WINDOW_WIDTH,
                height: DEFAULT_WINDOW_HEIGHT,
            },
            expanded_nodes: BTreeSet::new(),
            selection: None,
        }
    }
}

impl UiState {
    /// Load persisted UI state from `dir`.
    ///
    /// A missing state file restores defaults; malformed state returns an error
    /// so the shell can surface it instead of silently discarding operator state.
    pub fn load_from_dir(dir: impl AsRef<std::path::Path>) -> Result<Self, UiStateError> {
        let path = dir.as_ref().join(UI_STATE_FILE);
        match std::fs::read_to_string(&path) {
            Ok(raw) => {
                let state = toml::from_str(&raw).map_err(|source| UiStateError::Parse {
                    path: path.clone(),
                    source,
                })?;
                Ok(state)
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(UiStateError::Read { path, source }),
        }
    }

    /// Save UI state to `dir`.
    pub fn save_to_dir(&self, dir: impl AsRef<std::path::Path>) -> Result<(), UiStateError> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir).map_err(|source| UiStateError::CreateDir {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = dir.join(UI_STATE_FILE);
        let raw = toml::to_string_pretty(self).map_err(UiStateError::Serialize)?;
        std::fs::write(&path, raw).map_err(|source| UiStateError::Write { path, source })
    }
}

/// Errors raised while loading or saving persistent UI state.
#[derive(Debug, Error)]
pub enum UiStateError {
    #[error("failed to create UI state directory `{}`: {source}", path.display())]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to read UI state `{}`: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse UI state `{}`: {source}", path.display())]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("failed to serialize UI state: {0}")]
    Serialize(toml::ser::Error),
    #[error("failed to write UI state `{}`: {source}", path.display())]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("missing environment variable `{var}`")]
    MissingEnv { var: &'static str },
}

/// Return the default XDG state directory for `pohunek-gui`.
pub fn default_state_dir() -> Result<PathBuf, UiStateError> {
    if let Ok(value) = std::env::var("XDG_STATE_HOME") {
        if !value.is_empty() {
            return Ok(PathBuf::from(value).join("pohunek-gui"));
        }
    }
    match std::env::var("HOME") {
        Ok(value) if !value.is_empty() => Ok(PathBuf::from(value)
            .join(".local")
            .join("state")
            .join("pohunek-gui")),
        _ => Err(UiStateError::MissingEnv { var: "HOME" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_layout_fields_are_ignored_when_loading_legacy_state() {
        let raw = toml::to_string_pretty(&UiState::default()).expect("serialize default state");
        let raw = format!(
            "{raw}agents_pane_height = 360\nactive_tab = \"review\"\nopen_tabs = [\"agents\"]\n"
        );

        let parsed: UiState = toml::from_str(&raw).expect("legacy UI state parses");

        assert_eq!(parsed, UiState::default());
    }
}
