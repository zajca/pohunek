//! The `[rights]` table of a host profile.
//!
//! The table is read from the profile file the owner wrote, never from a
//! request. Two keys exist:
//!
//! - `owner_equivalent = true` gives the process tree of a session launched
//!   from the profile the rights of the owner's own shell. It is meant for
//!   manager profiles.
//! - `spawn_profiles = ["name", ...]` limits the profiles a session of this
//!   profile may launch to exactly those names. An empty list forbids
//!   spawning; an absent key leaves it unrestricted.
//!
//! A profile without the table has neither: its sessions keep the rights of
//! their own process tree and may spawn any profile.

use serde::Deserialize;

use super::{preset, read_profile_text, ProfileRegistry};
use crate::project::config::validate_name;

/// A parsed `[rights]` table.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawRights {
    #[serde(default)]
    owner_equivalent: bool,
    #[serde(default)]
    spawn_profiles: Option<Vec<String>>,
}

impl RawRights {
    /// Checks every `spawn_profiles` entry is a profile name.
    pub(super) fn validate(&self) -> Result<(), &'static str> {
        for name in self.spawn_profiles.iter().flatten() {
            validate_name("spawn profile", name).map_err(|_error| {
                "[rights] spawn_profiles holds an entry that is not a profile name"
            })?;
        }
        Ok(())
    }
}

/// The rights a profile grants the sessions launched from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProfileRights {
    /// The session's process tree has owner rights.
    pub(crate) owner_equivalent: bool,
    /// The profiles the session may launch; `None` is unrestricted.
    pub(crate) spawn_profiles: Option<Vec<String>>,
}

impl ProfileRights {
    /// A profile without a `[rights]` table.
    const fn unrestricted() -> Self {
        Self {
            owner_equivalent: false,
            spawn_profiles: None,
        }
    }

    /// The rights of a profile that cannot be read: no owner rights and no
    /// spawning.
    pub(crate) const fn closed() -> Self {
        Self {
            owner_equivalent: false,
            spawn_profiles: Some(Vec::new()),
        }
    }

    /// The rights a session holds when its launch rights are `self` and its
    /// profile file now reads `live`: nothing the file grew since the launch
    /// counts, and everything it took away does.
    pub(crate) fn narrowed_by(&self, live: &Self) -> Self {
        let spawn_profiles = match (&self.spawn_profiles, &live.spawn_profiles) {
            (Some(launch), Some(now)) => Some(
                launch
                    .iter()
                    .filter(|name| now.contains(name))
                    .cloned()
                    .collect(),
            ),
            (Some(list), None) | (None, Some(list)) => Some(list.clone()),
            (None, None) => None,
        };
        Self {
            owner_equivalent: self.owner_equivalent && live.owner_equivalent,
            spawn_profiles,
        }
    }

    /// Whether an owner-equivalent session with these rights may launch the
    /// profile `name`: any profile unless a spawn list names the allowed ones.
    pub(crate) fn allows_spawn_as_owner(&self, name: &str) -> bool {
        self.spawn_profiles
            .as_ref()
            .map_or(true, |allowed| allowed.iter().any(|entry| entry == name))
    }

    /// Whether a session with these rights may launch the profile `name`,
    /// whose own rights are `target`.
    ///
    /// A profile that grants owner rights is launched only when the list names
    /// it: an absent list leaves every other profile open, not that one.
    pub(crate) fn allows_spawn(&self, name: &str, target: &Self) -> bool {
        match &self.spawn_profiles {
            Some(allowed) => allowed.iter().any(|entry| entry == name),
            None => !target.owner_equivalent,
        }
    }
}

/// The rights of the agent `name`, read from its profile file.
///
/// A bare runtime name has no file and no table. A profile file the daemon
/// cannot read or parse grants nothing.
pub(crate) fn rights_of(registry: &ProfileRegistry, name: &str) -> ProfileRights {
    rights_at(registry, name, 0)
}

/// Most preset links [`rights_of`] follows to a full profile.
const MAX_PRESET_DEPTH: u8 = 4;

fn rights_at(registry: &ProfileRegistry, name: &str, depth: u8) -> ProfileRights {
    let Some(dir) = registry.dir.as_deref() else {
        return ProfileRights::unrestricted();
    };
    let path = dir.join(format!("{name}.toml"));
    let Ok(content) = read_profile_text(dir, name, &path) else {
        // Only a name without any file is a bare runtime; a file that exists
        // but cannot be read (unsafe mode, too large, unreadable) grants
        // nothing.
        let absent = matches!(
            std::fs::symlink_metadata(&path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        );
        let is_runtime = name
            .parse::<protocol::RuntimeId>()
            .is_ok_and(|id| registry.runtimes().resolve_id(&id).is_ok());
        return if absent && is_runtime {
            ProfileRights::unrestricted()
        } else {
            ProfileRights::closed()
        };
    };
    match preset::parse(name, &content) {
        Ok(preset::RawFile::Profile(raw)) => match raw.rights {
            Some(rights) if rights.validate().is_ok() => ProfileRights {
                owner_equivalent: rights.owner_equivalent,
                spawn_profiles: rights.spawn_profiles,
            },
            Some(_) => ProfileRights::closed(),
            None => ProfileRights::unrestricted(),
        },
        // A preset launches its parent, so it has the parent's rights.
        Ok(preset::RawFile::Preset(raw)) if depth < MAX_PRESET_DEPTH => {
            rights_at(registry, raw.parent(), depth + 1)
        }
        Ok(preset::RawFile::Preset(_)) => ProfileRights::closed(),
        Err(_error) => ProfileRights::closed(),
    }
}
