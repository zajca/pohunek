//! Persisted UI layout, selection, tree-node, and detail-tab view-model types.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use protocol::SessionId;

use crate::{
    HostId, DEFAULT_AGENTS_PANE_HEIGHT, DEFAULT_LEFT_PANE_WIDTH, DEFAULT_WINDOW_HEIGHT,
    DEFAULT_WINDOW_WIDTH, MIN_AGENTS_PANE_HEIGHT, UI_STATE_FILE,
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

/// The right pane's persistent tab, restored across GUI restarts.
///
/// `Detail` is the selection-driven session/project/host/start pane; the other
/// three promote what used to be stacked inside the project pane
/// (`linear_provider_view`, `github_provider_view`, `project_worktrees`) to
/// full tab bodies scoped to the current project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RightTab {
    Detail,
    Linear,
    GitHub,
    Worktrees,
}

/// Restores a persisted `active_tab` value, tolerating UI state written before
/// the tab-bar rework (B2): the legacy `DetailTab` variants (`session`,
/// `agents`, `project`) and any other unrecognized string normalize to
/// `Detail` instead of failing the whole load.
fn deserialize_right_tab<'de, D>(deserializer: D) -> Result<RightTab, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(match raw.as_str() {
        "linear" => RightTab::Linear,
        "git_hub" => RightTab::GitHub,
        "worktrees" => RightTab::Worktrees,
        _ => RightTab::Detail,
    })
}

/// Default `active_tab` for UI state files that predate the tab bar.
fn default_right_tab() -> RightTab {
    RightTab::Detail
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
    pub agents_pane_height: u16,
    pub window_size: WindowSize,
    pub expanded_nodes: BTreeSet<TreeNodeId>,
    pub selection: Option<Selection>,
    #[serde(
        default = "default_right_tab",
        deserialize_with = "deserialize_right_tab"
    )]
    pub active_tab: RightTab,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            left_pane_width: DEFAULT_LEFT_PANE_WIDTH,
            agents_pane_height: DEFAULT_AGENTS_PANE_HEIGHT,
            window_size: WindowSize {
                width: DEFAULT_WINDOW_WIDTH,
                height: DEFAULT_WINDOW_HEIGHT,
            },
            expanded_nodes: BTreeSet::new(),
            selection: None,
            active_tab: RightTab::Detail,
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
                Ok(normalize_loaded_ui_state(state))
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

fn normalize_loaded_ui_state(mut state: UiState) -> UiState {
    state.agents_pane_height = state.agents_pane_height.max(MIN_AGENTS_PANE_HEIGHT);
    state
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

    /// Builds a UI-state TOML string that mimics a file written before the B2
    /// tab-bar rework: it carries the removed `open_tabs` field and an
    /// `active_tab` value drawn from the old `DetailTab` vocabulary.
    fn legacy_ui_state_toml(legacy_active_tab: &str) -> String {
        let raw = toml::to_string_pretty(&UiState::default()).expect("serialize default state");
        let needle = "active_tab = \"detail\"";
        assert!(raw.contains(needle), "expected default active_tab line");
        let mut raw = raw.replace(needle, &format!("active_tab = \"{legacy_active_tab}\""));
        raw.push_str("open_tabs = [\"session\", \"agents\"]\n");
        raw
    }

    #[test]
    fn legacy_detail_tab_values_normalize_to_detail() {
        for legacy in ["session", "agents", "project", "not-a-real-tab"] {
            let raw = legacy_ui_state_toml(legacy);
            let state: UiState = toml::from_str(&raw)
                .unwrap_or_else(|err| panic!("legacy UI state `{legacy}` should parse: {err}"));
            assert_eq!(state.active_tab, RightTab::Detail);
        }
    }

    #[test]
    fn current_right_tab_values_round_trip() {
        for tab in [
            RightTab::Detail,
            RightTab::Linear,
            RightTab::GitHub,
            RightTab::Worktrees,
        ] {
            let state = UiState {
                active_tab: tab,
                ..UiState::default()
            };
            let raw = toml::to_string_pretty(&state).expect("serialize UI state");
            let parsed: UiState = toml::from_str(&raw).expect("deserialize UI state");
            assert_eq!(parsed.active_tab, tab);
        }
    }

    #[test]
    fn missing_active_tab_field_defaults_to_detail() {
        let raw = toml::to_string_pretty(&UiState::default()).expect("serialize default state");
        let raw: String = raw
            .lines()
            .filter(|line| !line.starts_with("active_tab"))
            .collect::<Vec<_>>()
            .join("\n");
        let state: UiState = toml::from_str(&raw).expect("UI state without active_tab parses");
        assert_eq!(state.active_tab, RightTab::Detail);
    }

    #[test]
    fn default_ui_state_opens_on_detail_tab() {
        assert_eq!(UiState::default().active_tab, RightTab::Detail);
    }
}
