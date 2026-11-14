//! Profile presets: a named selection of a parent profile's options.
//!
//! A preset file holds a `[preset]` table naming its parent host profile, the
//! option choices it selects and whether a launch may change them, plus the
//! presentation keys `display_name` and `audience`. It holds no launch input
//! of its own: a preset launches exactly as its parent with the merged
//! selection (the parent's defaults, then the preset's choices, then the
//! request), under its own name and labels. The revision of a preset session
//! is therefore the parent's revision text followed by the parent's name, so
//! editing the parent's selected choices, the selection or the parent needs
//! a decision, while editing the preset's labels or `locked` does not.
//!
//! A preset never names another preset or a bare runtime. Diagnostics name
//! the preset, its parent and option or choice ids, never a value.

// Rust guideline compliant 2026-11-14

use std::collections::BTreeMap;
use std::path::Path;

use protocol::ProtocolError;
use serde::{Deserialize, Serialize};

use super::options::{self, OptionRequest};
use super::presentation::{ProfileAudience, ProfileDisplayName};
use super::{invalid_profile, toml_diagnostic, ProfileRegistry, RawProfile};
use crate::project::config::validate_name;

/// Root key whose table makes a profile file a preset.
const PRESET_KEY: &str = "preset";

/// The root keys a preset file may hold.
const PRESET_ROOT_KEYS: [&str; 3] = [PRESET_KEY, "display_name", "audience"];

/// Line that separates the parent's revision text from the preset's part.
/// A `#` line cannot be part of a TOML value, so no profile text above it
/// produces the same bytes.
const PRESET_MARKER: &str = "\n# pohunek:preset-of\n";

/// A parsed preset file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawPreset {
    /// The parent and the selection.
    preset: RawPresetTable,
    /// Human label of the preset; absent ⇒ the preset shows by name.
    #[serde(default)]
    display_name: Option<ProfileDisplayName>,
    /// Picker audience of the preset; absent ⇒ `owner`.
    #[serde(default)]
    audience: Option<ProfileAudience>,
}

/// The `[preset]` table of a preset file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPresetTable {
    /// Name of the host profile the preset launches.
    parent: String,
    /// Whether a launch of the preset refuses an option selection.
    #[serde(default)]
    locked: bool,
    /// Choice id by option id; an option it does not name takes the
    /// parent's default.
    #[serde(default)]
    options: BTreeMap<String, String>,
}

/// What a profile file holds.
pub(super) enum RawFile {
    /// A full profile.
    Profile(Box<RawProfile>),
    /// A preset of another profile.
    Preset(RawPreset),
}

/// The parent of a resolved preset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PresetOf {
    /// Name of the parent host profile.
    pub(crate) parent: String,
    /// Whether a launch of the preset refuses an option selection.
    pub(crate) locked: bool,
}

impl PresetOf {
    /// The marker line and the compact JSON `{"parent":"<parent>"}`, as the
    /// revision text of a preset session appends them after the parent's.
    ///
    /// # Errors
    ///
    /// A fixed message when the parent name does not encode.
    pub(super) fn revision_suffix(&self) -> Result<String, String> {
        #[derive(Serialize)]
        struct Suffix<'a> {
            parent: &'a str,
        }

        let json = serde_json::to_string(&Suffix {
            parent: &self.parent,
        })
        .map_err(|_error| "the preset parent does not encode".to_owned())?;
        Ok(format!("{PRESET_MARKER}{json}"))
    }
}

/// Parses the file text `content` of profile `name` as a profile or, when
/// its root holds a `preset` table, as a preset.
///
/// # Errors
///
/// `invalid_profile` for text that does not parse, a preset that holds a
/// launch key, and a `preset` key that is not a table.
pub(super) fn parse(name: &str, content: &str) -> Result<RawFile, ProtocolError> {
    let root: toml::Table = toml::from_str(content)
        .map_err(|err| invalid_profile(name, &toml_diagnostic(content, &err)))?;
    let Some(preset) = root.get(PRESET_KEY) else {
        let raw: RawProfile = toml::from_str(content)
            .map_err(|err| invalid_profile(name, &toml_diagnostic(content, &err)))?;
        return Ok(RawFile::Profile(Box::new(raw)));
    };
    if root
        .keys()
        .any(|key| !PRESET_ROOT_KEYS.contains(&key.as_str()))
    {
        return Err(invalid_profile(
            name,
            "a preset holds only [preset], display_name and audience",
        ));
    }
    if !preset.is_table() {
        return Err(invalid_profile(
            name,
            "preset must be a table that names the parent",
        ));
    }
    let raw: RawPreset = toml::from_str(content)
        .map_err(|err| invalid_profile(name, &toml_diagnostic(content, &err)))?;
    Ok(RawFile::Preset(raw))
}

/// Whether the profile text `text` is a preset: its root holds a `preset`
/// key. Text that does not parse is not one.
pub(crate) fn is_preset(text: &str) -> bool {
    toml::from_str::<toml::Table>(text).is_ok_and(|root| root.contains_key(PRESET_KEY))
}

/// The profile file whose text launches the profile `name`: the file itself,
/// or the parent of the preset `name`.
pub(super) struct LaunchSource {
    /// Name of the profile the text belongs to.
    pub(super) name: String,
    /// Its file text (the planned text when a release step rewrites it).
    pub(super) content: String,
    /// Its parsed keys.
    pub(super) raw: Box<RawProfile>,
    /// The preset `name` is, when it is one.
    pub(super) preset: Option<PresetOf>,
}

/// Reads the text that launches the profile `name` from the agents
/// directory `dir`, through the registry's planned texts.
///
/// # Errors
///
/// The read and parse errors of the file, and those of
/// [`parent_of`] for a preset.
pub(super) fn launch_source(
    registry: &ProfileRegistry,
    dir: &Path,
    name: &str,
) -> Result<LaunchSource, ProtocolError> {
    let path = dir.join(format!("{name}.toml"));
    let content = registry.profile_text(dir, name, &path)?;
    match parse(name, &content)? {
        RawFile::Profile(raw) => Ok(LaunchSource {
            name: name.to_owned(),
            content,
            raw,
            preset: None,
        }),
        RawFile::Preset(preset) => {
            let (content, raw) = parent_of(registry, dir, name, &preset)?;
            let parent = preset.preset.parent;
            Ok(LaunchSource {
                name: parent.clone(),
                content,
                raw,
                preset: Some(PresetOf {
                    parent,
                    locked: preset.preset.locked,
                }),
            })
        }
    }
}

/// The file text and keys of the parent of preset `name`.
///
/// # Errors
///
/// `invalid_profile` naming the preset for a parent that is not a valid
/// profile name, is no host profile file (a bare runtime id or a missing
/// file) or is itself a preset; the parent's own read and parse errors name
/// the parent.
fn parent_of(
    registry: &ProfileRegistry,
    dir: &Path,
    name: &str,
    preset: &RawPreset,
) -> Result<(String, Box<RawProfile>), ProtocolError> {
    let parent = preset.preset.parent.as_str();
    if validate_name("agent", parent).is_err() {
        return Err(invalid_profile(
            name,
            "preset.parent is not a valid profile name",
        ));
    }
    let path = dir.join(format!("{parent}.toml"));
    if !registry.planned.contains_key(parent) && !path.is_file() {
        return Err(invalid_profile(
            name,
            &format!("preset parent '{parent}' is not a host profile file"),
        ));
    }
    let content = registry.profile_text(dir, parent, &path)?;
    match parse(parent, &content)? {
        RawFile::Profile(raw) => Ok((content, raw)),
        RawFile::Preset(_) => Err(invalid_profile(
            name,
            &format!("preset parent '{parent}' is a preset; a preset cannot name another preset"),
        )),
    }
}

/// Checks that every option and choice the preset `name` selects is declared
/// by its parent profile `parent`.
fn check_choices(
    name: &str,
    parent: &str,
    selected: &BTreeMap<String, String>,
    declared: &BTreeMap<String, options::RawOption>,
) -> Result<(), ProtocolError> {
    for (option, choice) in selected {
        if !options::is_option_id(option) || !options::is_option_id(choice) {
            return Err(invalid_profile(
                name,
                "a preset option or choice id is not a valid name",
            ));
        }
        let Some(declared) = declared.get(option) else {
            return Err(invalid_profile(
                name,
                &format!("preset option '{option}' is not declared by parent '{parent}'"),
            ));
        };
        if !declared.offers(choice) {
            return Err(invalid_profile(
                name,
                &format!(
                    "preset option '{option}' selects choice '{choice}', which parent '{parent}' does not declare"
                ),
            ));
        }
    }
    Ok(())
}

/// Resolves the preset `name` read as `preset`: its parent resolved with
/// the merged selection, under the preset's name and labels.
///
/// `Defaults` takes the preset's choices over the parent's defaults and a
/// `Requested` map overrides them; a locked preset refuses a non-empty
/// request. A `Recorded` map is complete, so the preset's own choices play
/// no part and are not checked: an existing session relaunches with what it
/// recorded.
///
/// # Errors
///
/// The errors of [`parent_of`], `invalid_profile` for a choice the parent
/// does not declare, `agent_options_locked` for a selection sent to a locked
/// preset, `agent_options_not_declared` for one sent to a preset of a parent
/// without options, and the parent's resolution errors.
pub(super) fn resolve(
    registry: &ProfileRegistry,
    dir: &Path,
    name: &str,
    preset: &RawPreset,
    pinned: Option<&std::sync::Arc<super::RuntimeDefinition>>,
    request: OptionRequest<'_>,
) -> Result<super::ResolvedAgent, ProtocolError> {
    let (content, raw) = parent_of(registry, dir, name, preset)?;
    let parent = preset.preset.parent.as_str();
    let own = &preset.preset.options;
    let selects = !matches!(request, OptionRequest::Recorded(_));
    if selects {
        check_choices(name, parent, own, &raw.options)?;
    }
    let merged: BTreeMap<String, String>;
    let request = match request {
        OptionRequest::Defaults => OptionRequest::Requested(own),
        OptionRequest::Requested(map) if map.is_empty() => OptionRequest::Requested(own),
        OptionRequest::Requested(map) => {
            if preset.preset.locked {
                return Err(ProtocolError::agent_options_locked(name));
            }
            if raw.options.is_empty() {
                return Err(ProtocolError::agent_options_not_declared(name));
            }
            merged = own
                .iter()
                .chain(map)
                .map(|(option, choice)| (option.clone(), choice.clone()))
                .collect();
            OptionRequest::Requested(&merged)
        }
        recorded @ OptionRequest::Recorded(_) => recorded,
    };
    let of = PresetOf {
        parent: parent.to_owned(),
        locked: preset.preset.locked,
    };
    let mut agent = super::resolve_raw(
        super::ParsedProfile {
            name: parent,
            content: &content,
            raw: *raw,
        },
        dir,
        &registry.runtimes,
        pinned,
        request,
        Some(&of),
    )?;
    name.clone_into(&mut agent.name);
    if let Some(profile) = agent.profile.as_mut() {
        profile.display_name = preset
            .display_name
            .as_ref()
            .map(|label| label.as_str().to_owned());
        profile.audience = preset.audience.map(ProfileAudience::get);
        if selects {
            // The preset's choices are its defaults, as `host.inspect` shows.
            for option in &mut profile.options {
                if let Some(choice) = own.get(&option.id) {
                    choice.clone_into(&mut option.default);
                }
            }
        }
        profile.preset = Some(of);
    }
    Ok(agent)
}

/// The `display_name` a preset file sets, for the label-only read.
pub(super) fn display_name(preset: &RawPreset) -> Option<&str> {
    preset.display_name.as_ref().map(ProfileDisplayName::as_str)
}

/// The `[preset] parent` that the profile text `text` names, read leniently:
/// any TOML text whose `preset` table holds a string `parent` names it, even
/// when another key of the preset is invalid, so a preset that a fix would
/// make launchable still counts. Other text names no parent.
fn named_parent(text: &str) -> Option<String> {
    let root = toml::from_str::<toml::Table>(text).ok()?;
    root.get(PRESET_KEY)?
        .as_table()?
        .get("parent")?
        .as_str()
        .map(ToOwned::to_owned)
}

impl ProfileRegistry {
    /// The presets whose `[preset] parent` satisfies `is_parent`, sorted by
    /// name.
    ///
    /// Reads the agents directory with the bounded scan of
    /// [`Self::scanned_texts`]; a file that cannot be read names no parent.
    /// The caller decides what a parent name stands for, so a parent that
    /// reaches a profile through another spelling or an in-tree link can count
    /// as naming it.
    ///
    /// # Errors
    ///
    /// The listing errors of [`Self::scanned_texts`], so a directory that
    /// cannot be listed completely is never taken to hold no preset.
    pub(crate) fn presets_of(
        &self,
        is_parent: impl Fn(&str) -> bool,
    ) -> std::io::Result<Vec<String>> {
        let mut presets: Vec<String> = self
            .scanned_texts()?
            .into_iter()
            .filter(|(_name, text)| {
                text.as_deref()
                    .ok()
                    .and_then(named_parent)
                    .is_some_and(|named| is_parent(&named))
            })
            .map(|(name, _text)| name)
            .collect();
        presets.sort();
        Ok(presets)
    }
}
