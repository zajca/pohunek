//! Where an agent reads user-level skills, as a package descriptor declares it.
//!
//! The descriptor names a closed root (the agent's config home or the user's
//! home) and a bounded relative directory; core owns the content, the file
//! layout below that directory and every write. Nothing here reads or writes
//! the filesystem.

// Rust guideline compliant 2026-11-11

use std::ffi::OsString;
use std::path::PathBuf;

use super::config_home::{ConfigHome, HomeError};
use super::definition::DefinitionError;
use crate::agent::is_plain_relative_path;

/// Descriptor value of [`AgentSkillRoot::ConfigHome`].
const ROOT_CONFIG_HOME: &str = "config_home";

/// Descriptor value of [`AgentSkillRoot::UserHome`].
const ROOT_USER_HOME: &str = "user_home";

/// Last component of a `config_home`-rooted skills directory.
const SKILLS_DIR_NAME: &str = "skills";

/// The only directory a `user_home` root accepts: the location several agents
/// share below the user's home.
const USER_HOME_SKILLS_DIR: &str = ".agents/skills";

/// Environment variable that names the user's home directory.
const HOME_ENV: &str = "HOME";

/// What the skills directory is relative to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSkillRoot {
    /// The agent's config home: the declared variable, else its default.
    ConfigHome,
    /// The user's home directory. The location is shared by every home and
    /// profile of the agent on the host.
    UserHome,
}

/// A validated `[agent_skill]` declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSkill {
    root: AgentSkillRoot,
    dir: String,
}

impl AgentSkill {
    /// Validates the closed root and the relative skills directory.
    ///
    /// # Errors
    ///
    /// Returns a [`DefinitionError::Field`] when `root` is not
    /// `config_home` or `user_home`, `dir` is not a bounded relative path of
    /// plain components, a `config_home` directory does not end in `skills`, or
    /// a `user_home` directory is not `.agents/skills`.
    pub fn new(root: &str, dir: &str) -> Result<Self, DefinitionError> {
        let root = match root {
            ROOT_CONFIG_HOME => AgentSkillRoot::ConfigHome,
            ROOT_USER_HOME => AgentSkillRoot::UserHome,
            _ => {
                return Err(DefinitionError::Field {
                    field: "agent_skill.root",
                    reason: "must be \"config_home\" or \"user_home\"",
                })
            }
        };
        if !is_plain_relative_path(dir) {
            return Err(DefinitionError::Field {
                field: "agent_skill.dir",
                reason: "must be a relative path of plain components",
            });
        }
        let accepted = match root {
            AgentSkillRoot::ConfigHome => dir.rsplit('/').next() == Some(SKILLS_DIR_NAME),
            AgentSkillRoot::UserHome => dir == USER_HOME_SKILLS_DIR,
        };
        if !accepted {
            return Err(DefinitionError::Field {
                field: "agent_skill.dir",
                reason: "a config_home directory must end in \"skills\" and a user_home directory must be \".agents/skills\"",
            });
        }
        Ok(Self {
            root,
            dir: dir.to_owned(),
        })
    }

    /// What the directory is relative to.
    #[must_use]
    pub fn root(&self) -> AgentSkillRoot {
        self.root
    }

    /// The skills directory below the root.
    #[must_use]
    pub fn dir(&self) -> &str {
        &self.dir
    }

    /// Whether every home and profile of the agent shares the location.
    #[must_use]
    pub fn is_shared(&self) -> bool {
        self.root == AgentSkillRoot::UserHome
    }

    /// The absolute skills directory for the environment `lookup` describes.
    ///
    /// `lookup` is the environment the launched agent sees. Neither the config
    /// home nor `HOME` is expanded: a relative or non-UTF-8 value is refused.
    ///
    /// # Errors
    ///
    /// Returns a [`HomeError`] when the root cannot be resolved to an absolute
    /// UTF-8 path, and [`HomeError::Unset`] naming the variable the root needs
    /// when the definition declares no config home for a `config_home` root.
    pub fn resolve(
        &self,
        config_home: Option<&ConfigHome>,
        lookup: &dyn Fn(&str) -> Option<OsString>,
    ) -> Result<PathBuf, HomeError> {
        let root = match self.root {
            AgentSkillRoot::ConfigHome => config_home
                .ok_or_else(|| HomeError::Unset {
                    env: HOME_ENV.to_owned(),
                })?
                .resolve(lookup)?,
            AgentSkillRoot::UserHome => {
                let home = lookup(HOME_ENV)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| HomeError::Unset {
                        env: HOME_ENV.to_owned(),
                    })?;
                let path = PathBuf::from(home);
                if !path.is_absolute() || path.to_str().is_none() {
                    return Err(HomeError::Invalid {
                        variable: HOME_ENV.to_owned(),
                    });
                }
                path
            }
        };
        Ok(root.join(&self.dir))
    }
}
