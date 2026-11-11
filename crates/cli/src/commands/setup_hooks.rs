//! The hook selection of bare `pohunek setup`: which agent homes the run
//! installs hooks into.
//!
//! A home is named `agent` for the agent's default home and `agent:profile`
//! for the home a host profile selects. Hermes homes use the same names:
//! `hermes` is its default home and `hermes:<profile>` a named profile.

// Rust guideline compliant 2026-10-14

use std::fmt;
use std::str::FromStr;

use protocol::RuntimeId;

/// Spelling of the selection that installs hooks in every home.
const SELECT_ALL: &str = "all";

/// Spelling of the selection that installs no hook.
const SELECT_NONE: &str = "none";

/// Separates the agent from the profile in a home name.
const PROFILE_SEPARATOR: char = ':';

/// Separates the homes of a selection.
const HOME_SEPARATOR: char = ',';

/// One selected agent home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookHome {
    /// Runtime id of the agent.
    agent: String,
    /// Host profile whose home is selected; `None` for the default home.
    profile: Option<String>,
}

impl HookHome {
    /// The label a plan entry for this home carries: `agent` or
    /// `agent profile <name>`.
    fn label(&self) -> String {
        home_label(&self.agent, self.profile.as_deref())
    }
}

impl fmt::Display for HookHome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.profile {
            Some(profile) => write!(f, "{}{PROFILE_SEPARATOR}{profile}", self.agent),
            None => f.write_str(&self.agent),
        }
    }
}

/// The label the plan and the install report give a home.
pub(crate) fn home_label(agent: &str, profile: Option<&str>) -> String {
    match profile {
        Some(profile) => format!("{agent} profile {profile}"),
        None => agent.to_owned(),
    }
}

/// Which agent homes a setup run installs hooks into.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum HookSelection {
    /// Every home the plan lists.
    #[default]
    All,
    /// No home.
    None,
    /// Exactly these homes.
    Homes(Vec<HookHome>),
}

impl FromStr for HookSelection {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            SELECT_ALL => return Ok(Self::All),
            SELECT_NONE => return Ok(Self::None),
            _ => {}
        }
        let mut homes: Vec<HookHome> = Vec::new();
        for part in value.split(HOME_SEPARATOR) {
            let (agent, profile) = match part.split_once(PROFILE_SEPARATOR) {
                Some((agent, profile)) => (agent, Some(profile)),
                None => (part, None),
            };
            if profile.is_some_and(|name| name.is_empty() || name.contains(PROFILE_SEPARATOR)) {
                return Err(format!("`{part}` is not `agent` or `agent:profile`"));
            }
            RuntimeId::parse(agent)
                .map_err(|error| format!("`{agent}` is not a runtime id ({error})"))?;
            let home = HookHome {
                agent: agent.to_owned(),
                profile: profile.map(str::to_owned),
            };
            if !homes.contains(&home) {
                homes.push(home);
            }
        }
        Ok(Self::Homes(homes))
    }
}

impl HookSelection {
    /// Whether the home labelled `label` is selected.
    ///
    /// `agent_family` is the agent of a label that is not itself a home, such
    /// as the Hermes profiles directory that cannot be listed: it is selected
    /// whenever any home of that agent is.
    pub(crate) fn selects(&self, label: &str, agent_family: &str) -> bool {
        match self {
            Self::All => true,
            Self::None => false,
            Self::Homes(homes) => homes.iter().any(|home| {
                home.label() == label
                    || (home.agent == agent_family && label == family_label(agent_family))
            }),
        }
    }

    /// Whether the selection is `none`.
    pub(crate) fn is_none(&self) -> bool {
        *self == Self::None
    }

    /// Checks that every selected home is one of `available` (plan labels).
    ///
    /// # Errors
    ///
    /// Returns the message naming the selected homes the plan does not list
    /// and the homes it does.
    pub(crate) fn validate(&self, available: &[String]) -> Result<(), String> {
        let Self::Homes(homes) = self else {
            return Ok(());
        };
        let unknown: Vec<String> = homes
            .iter()
            .filter(|home| !available.contains(&home.label()))
            .map(ToString::to_string)
            .collect();
        if unknown.is_empty() {
            return Ok(());
        }
        let listed: Vec<String> = available
            .iter()
            .filter(|label| !label.ends_with(" profiles"))
            .map(|label| label.replacen(" profile ", &PROFILE_SEPARATOR.to_string(), 1))
            .collect();
        Err(format!(
            "--hooks names {}, which this host has no agent home for; the homes it has are: {}",
            unknown.join(", "),
            if listed.is_empty() {
                "none".to_owned()
            } else {
                listed.join(", ")
            }
        ))
    }
}

/// The label of the entry that stands for a whole agent family.
fn family_label(agent: &str) -> String {
    format!("{agent} profiles")
}
