//! Profile options: closed enumerations of launch choices.
//!
//! A profile may declare `[options.<id>]` tables. Each option has a label, a
//! default choice and a closed set of choices; a choice adds launch `args`,
//! `env` entries and an optional `min_runtime` floor. A resolution takes one
//! choice per option (the default unless a request or a session record names
//! another): the selected choices' args follow the profile `args` in
//! option-id order and their env joins the profile `[env]`, so every check a
//! launch applies to profile args and env applies to them. Every value comes
//! from the owner's profile file; a selection carries ids only.
//!
//! A choice env key or value, or a choice argument, that looks like a
//! credential fails the load. Diagnostics name option and choice ids, env
//! keys and argument indexes, never a value, label text or an id that fails
//! the id rule.

// Rust guideline compliant 2026-11-14

use std::collections::{BTreeMap, HashSet};

use package::version_probe::ProbeVersion;
use protocol::{
    ProtocolError, MAX_PROFILE_OPTIONS, MAX_PROFILE_OPTION_CHOICES, MAX_PROFILE_OPTION_ID_BYTES,
    MAX_PROFILE_OPTION_LABEL_BYTES,
};
use serde::{Deserialize, Serialize};

use super::{invalid_profile, EnvValue, LAUNCH_OWNED_ENV_NAMES, LOADER_ENV_PREFIXES};
use crate::agent::{
    credential_arg_problem, credential_literal_problem, credential_name_problem, env_name_problem,
    MAX_EXISTENCE_TEXT_BYTES,
};
use crate::project::config::validate_name;

/// Line that separates the profile text from the selected choices in the
/// revision text. A `#` line cannot be part of a TOML value, so the profile
/// text above it never produces the same bytes.
const SELECTED_OPTIONS_MARKER: &str = "\n# pohunek:selected-options\n";

/// Which choice each option of a profile takes in one resolution.
#[derive(Debug, Clone, Copy)]
pub(crate) enum OptionRequest<'a> {
    /// Every option takes its default: a launch that selects nothing, and
    /// every resolution that reads only what options cannot change (the base
    /// runtime, its definition, the manifest, native resume, the config home).
    Defaults,
    /// A `session.new` selection by option id. Every id must name a declared
    /// option and one of its choices; an option it does not name takes its
    /// default.
    #[expect(dead_code, reason = "first sent by session.new")]
    Requested(&'a BTreeMap<String, String>),
    /// The selection a session recorded. An option or choice id the profile
    /// no longer declares is refused and never replaced by another choice;
    /// an option it does not name takes its default.
    Recorded(&'a BTreeMap<String, String>),
}

/// One `[options.<id>]` table of a profile file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawOption {
    /// Picker label.
    label: String,
    /// Choice id the option takes when nothing selects another.
    default: String,
    /// The closed set of choices, by choice id.
    choices: BTreeMap<String, RawChoice>,
}

/// One `choices.<id>` entry of a profile option.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawChoice {
    /// Picker entry label.
    label: String,
    /// Launch args appended after the profile args.
    #[serde(default)]
    args: Vec<String>,
    /// PTY env entries added to the profile `[env]`.
    #[serde(default)]
    env: BTreeMap<String, EnvValue>,
    /// Lowest runtime version the choice needs, as a probe version.
    #[serde(default)]
    min_runtime: Option<String>,
}

/// A validated profile option, as `host.inspect` reports it.
#[derive(Debug, Clone)]
#[expect(dead_code, reason = "first read by host.inspect")]
pub(crate) struct DeclaredOption {
    /// Option id.
    pub(crate) id: String,
    /// Trimmed picker label.
    pub(crate) label: String,
    /// Id of the default choice.
    pub(crate) default: String,
    /// The choices, in id order.
    pub(crate) choices: Vec<DeclaredChoice>,
}

/// A validated choice of a [`DeclaredOption`].
#[derive(Debug, Clone)]
#[expect(dead_code, reason = "first read by host.inspect")]
pub(crate) struct DeclaredChoice {
    /// Choice id.
    pub(crate) id: String,
    /// Trimmed picker entry label.
    pub(crate) label: String,
    /// Lowest runtime version the choice needs.
    pub(crate) min_runtime: Option<ProbeVersion>,
}

/// The env names no choice may set, besides the fixed launch-owned sets.
pub(super) struct ReservedEnv<'a> {
    /// The base runtime's config home variable.
    pub(super) config_home: Option<&'a str>,
    /// The base runtime's `[launch_env]` pairs.
    pub(super) launch_env: &'a [(String, String)],
    /// The profile's `secret_env` slots.
    pub(super) secret_slots: &'a [String],
    /// Every key of the profile's `[env]` table.
    pub(super) profile_env: &'a HashSet<String>,
}

/// Validates the `[options]` of profile `name` and returns them in id order.
///
/// The counts and ids of every option are checked before any label, default
/// or choice body, so a malformed table is reported by its shape. An env key
/// may be set by several choices of one option, which are alternatives, but
/// by no choice of another option.
///
/// # Errors
///
/// `invalid_profile` naming the option, choice, env key or argument index
/// that breaks a rule.
pub(super) fn declare(
    name: &str,
    options: &BTreeMap<String, RawOption>,
    reserved: &ReservedEnv<'_>,
) -> Result<Vec<DeclaredOption>, ProtocolError> {
    check_shape(name, options)?;
    // Env key -> the option whose choices set it.
    let mut owners: BTreeMap<&str, &str> = BTreeMap::new();
    let mut declared = Vec::with_capacity(options.len());
    for (id, option) in options {
        let label = checked_label(&option.label)
            .ok_or_else(|| invalid_profile(name, &format!("option '{id}': {}", label_rule())))?;
        if !option.choices.contains_key(&option.default) {
            return Err(default_missing(name, id));
        }
        let mut choices = Vec::with_capacity(option.choices.len());
        for (choice_id, choice) in &option.choices {
            let at = format!("option '{id}' choice '{choice_id}'");
            choices.push(declare_choice(
                name, &at, choice_id, choice, reserved, &owners,
            )?);
        }
        for choice in option.choices.values() {
            for key in choice.env.keys() {
                owners.insert(key.as_str(), id.as_str());
            }
        }
        declared.push(DeclaredOption {
            id: id.clone(),
            label,
            default: option.default.clone(),
            choices,
        });
    }
    Ok(declared)
}

/// Checks the option count and, per option, its id, choice count and choice
/// ids.
fn check_shape(name: &str, options: &BTreeMap<String, RawOption>) -> Result<(), ProtocolError> {
    if options.len() > MAX_PROFILE_OPTIONS {
        return Err(invalid_profile(
            name,
            &format!(
                "the profile declares {} options; at most {MAX_PROFILE_OPTIONS} are allowed",
                options.len()
            ),
        ));
    }
    for (id, option) in options {
        if !is_option_id(id) {
            return Err(invalid_profile(
                name,
                &format!("an option id is not a valid name ({})", id_rule()),
            ));
        }
        let count = option.choices.len();
        if count == 0 || count > MAX_PROFILE_OPTION_CHOICES {
            return Err(invalid_profile(
                name,
                &format!(
                    "option '{id}' declares {count} choices; 1 to {MAX_PROFILE_OPTION_CHOICES} are allowed"
                ),
            ));
        }
        if option.choices.keys().any(|choice| !is_option_id(choice)) {
            return Err(invalid_profile(
                name,
                &format!(
                    "option '{id}': a choice id is not a valid name ({})",
                    id_rule()
                ),
            ));
        }
    }
    Ok(())
}

/// Validates the choice `id` that the load error locates as `at`. `owners`
/// maps every env key the options before this one set to its option.
fn declare_choice(
    name: &str,
    at: &str,
    id: &str,
    choice: &RawChoice,
    reserved: &ReservedEnv<'_>,
    owners: &BTreeMap<&str, &str>,
) -> Result<DeclaredChoice, ProtocolError> {
    let label = checked_label(&choice.label)
        .ok_or_else(|| invalid_profile(name, &format!("{at}: {}", label_rule())))?;
    for (key, value) in &choice.env {
        if let Some(reason) = env_key_problem(key, reserved) {
            return Err(invalid_profile(
                name,
                &format!("{at}: {} {reason}", shown_key(key)),
            ));
        }
        if let Some(owner) = owners.get(key.as_str()) {
            return Err(invalid_profile(
                name,
                &format!("{at}: env key '{key}' is also set by option '{owner}'"),
            ));
        }
        if let Some(problem) = credential_literal_problem(&value.0) {
            return Err(invalid_profile(
                name,
                &format!("{at}: the value of env key '{key}' {problem}"),
            ));
        }
    }
    if let Some(index) = credential_arg_problem(&choice.args) {
        return Err(invalid_profile(
            name,
            &format!("{at}: args[{index}] carries a credential"),
        ));
    }
    let min_runtime = choice
        .min_runtime
        .as_deref()
        .map(|text| {
            ProbeVersion::parse_revision(text).ok_or_else(|| {
                invalid_profile(name, &format!("{at}: min_runtime is not a version"))
            })
        })
        .transpose()?;
    Ok(DeclaredChoice {
        id: id.to_owned(),
        label,
        min_runtime,
    })
}

/// Takes one choice per option of profile `name` as `request` asks.
///
/// A bare runtime resolves with an empty `options` map, so a non-empty
/// selection for it is refused the same way as for a profile without
/// options.
///
/// # Errors
///
/// `agent_options_not_declared` for a non-empty request to a profile without
/// options, `agent_option_unknown` or `agent_option_choice_unknown` for a
/// request naming an undeclared id, `agent_option_choice_missing` for a
/// recorded id the profile no longer declares, and `invalid_profile` for an
/// option whose default is not one of its choices.
pub(super) fn select<'a>(
    name: &str,
    options: &'a BTreeMap<String, RawOption>,
    request: OptionRequest<'_>,
) -> Result<Selection<'a>, ProtocolError> {
    let named = match request {
        OptionRequest::Defaults => None,
        OptionRequest::Requested(map) => {
            if !map.is_empty() && options.is_empty() {
                return Err(ProtocolError::agent_options_not_declared(name));
            }
            for (option, choice) in map {
                let declared = options
                    .get(option)
                    .ok_or_else(|| ProtocolError::agent_option_unknown(option))?;
                if !declared.choices.contains_key(choice) {
                    return Err(ProtocolError::agent_option_choice_unknown(option, choice));
                }
            }
            Some(map)
        }
        OptionRequest::Recorded(map) => {
            for (option, choice) in map {
                let declared = options
                    .get(option)
                    .is_some_and(|declared| declared.choices.contains_key(choice));
                if !declared {
                    return Err(ProtocolError::agent_option_choice_missing(option, choice));
                }
            }
            Some(map)
        }
    };
    let mut picked = Vec::with_capacity(options.len());
    for (id, option) in options {
        let wanted = named.and_then(|map| map.get(id)).unwrap_or(&option.default);
        let (choice, body) = option
            .choices
            .get_key_value(wanted)
            .ok_or_else(|| default_missing(name, id))?;
        picked.push(Picked {
            option: id,
            choice,
            body,
        });
    }
    Ok(Selection { picked })
}

/// The choice each option of a profile took in one resolution, in option-id
/// order.
pub(super) struct Selection<'a> {
    picked: Vec<Picked<'a>>,
}

/// One option of a [`Selection`] with its chosen choice.
struct Picked<'a> {
    option: &'a str,
    choice: &'a str,
    body: &'a RawChoice,
}

impl<'a> Selection<'a> {
    /// Every option id with the choice id it took, defaults included.
    pub(super) fn ids(&self) -> BTreeMap<String, String> {
        self.picked
            .iter()
            .map(|picked| (picked.option.to_owned(), picked.choice.to_owned()))
            .collect()
    }

    /// The selected choices' args, in option-id order.
    pub(super) fn args(&self) -> impl Iterator<Item = &'a String> + '_ {
        self.picked
            .iter()
            .flat_map(|picked| picked.body.args.iter())
    }

    /// The selected choices' env entries, in option-id then key order.
    pub(super) fn env(&self) -> impl Iterator<Item = (String, String)> + '_ {
        self.picked.iter().flat_map(|picked| {
            picked
                .body
                .env
                .iter()
                .map(|(key, value)| (key.clone(), value.0.clone()))
        })
    }

    /// The selected choices as the profile revision digests them.
    pub(super) fn revision(&self) -> SelectedChoices {
        SelectedChoices(
            self.picked
                .iter()
                .map(|picked| SelectedChoiceView {
                    option: picked.option.to_owned(),
                    choice: picked.choice.to_owned(),
                    args: picked.body.args.clone(),
                    env: picked
                        .body
                        .env
                        .iter()
                        .map(|(key, value)| (key.clone(), value.0.clone()))
                        .collect(),
                    min_runtime: picked.body.min_runtime.clone(),
                })
                .collect(),
        )
    }
}

/// The selected choices of a resolution as the profile revision digests
/// them: option and choice ids with each choice's args, env and floor, in
/// option-id order.
///
/// Labels and defaults are left out, so relabelling a choice, changing a
/// default or adding a choice leaves the revision of a session that recorded
/// its choices unchanged, while editing the body of a selected choice moves
/// it. Secret-bearing (choice env values): never logged, stored or printed;
/// the revision text is only hashed.
pub(crate) struct SelectedChoices(Vec<SelectedChoiceView>);

/// One selected choice in the revision text.
#[derive(Serialize)]
struct SelectedChoiceView {
    option: String,
    choice: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    min_runtime: Option<String>,
}

impl SelectedChoices {
    /// The marker line and the compact JSON of the selected choices, as the
    /// revision text appends them after the profile text.
    ///
    /// # Errors
    ///
    /// A fixed message when the choices do not encode.
    pub(super) fn revision_suffix(&self) -> Result<String, String> {
        let json = serde_json::to_string(&self.0)
            .map_err(|_error| "the selected options do not encode".to_owned())?;
        Ok(format!("{SELECTED_OPTIONS_MARKER}{json}"))
    }
}

impl std::fmt::Debug for SelectedChoices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SelectedChoices({} choices, redacted)", self.0.len())
    }
}

/// Whether `id` is a valid option or choice id.
fn is_option_id(id: &str) -> bool {
    id.len() <= MAX_PROFILE_OPTION_ID_BYTES && validate_name("option", id).is_ok()
}

/// What an option or choice id may hold, as the load error states it.
fn id_rule() -> String {
    format!("letters, digits, '.', '_' and '-', at most {MAX_PROFILE_OPTION_ID_BYTES} bytes")
}

/// The rule an option or choice label breaks, as the load error states it.
fn label_rule() -> String {
    format!(
        "label must be 1 to {MAX_PROFILE_OPTION_LABEL_BYTES} bytes without control, separator or format characters"
    )
}

/// `label` trimmed, or `None` when it is empty, too long or holds a hidden
/// or separator character.
fn checked_label(label: &str) -> Option<String> {
    let trimmed = label.trim();
    let valid = !trimmed.is_empty()
        && trimmed.len() <= MAX_PROFILE_OPTION_LABEL_BYTES
        && !trimmed
            .chars()
            .any(pohunek_worker_protocol::is_hidden_or_separator);
    valid.then(|| trimmed.to_owned())
}

/// `invalid_profile` for option `id` of profile `name` whose default names
/// no choice.
fn default_missing(name: &str, id: &str) -> ProtocolError {
    invalid_profile(
        name,
        &format!("option '{id}': default is not one of its choices"),
    )
}

/// Why a choice may not set the env key `key`, or `None` when it may.
fn env_key_problem(key: &str, reserved: &ReservedEnv<'_>) -> Option<&'static str> {
    if let Some(reason) = env_name_problem(key) {
        Some(reason)
    } else if pohunek_worker_protocol::is_denylisted(key) {
        Some("is a service-manager variable")
    } else if LOADER_ENV_PREFIXES
        .iter()
        .any(|prefix| key.starts_with(prefix))
    {
        Some("is a dynamic-loader variable")
    } else if LAUNCH_OWNED_ENV_NAMES.contains(&key) {
        Some("is set by the launch itself")
    } else if reserved.config_home == Some(key) {
        Some("is the runtime's config home variable")
    } else if reserved.launch_env.iter().any(|(name, _)| name == key) {
        Some("is set by the runtime's [launch_env]")
    } else if reserved.secret_slots.iter().any(|slot| slot == key) {
        Some("is a secret_env slot of the profile")
    } else if reserved.profile_env.contains(key) {
        Some("is also a key of [env]")
    } else {
        credential_name_problem(key)
    }
}

/// How a load error names the env key `key`: quoted when it is a printable
/// variable name, else without its text.
fn shown_key(key: &str) -> String {
    let printable = !key.is_empty()
        && key.len() <= MAX_EXISTENCE_TEXT_BYTES
        && key
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_');
    if printable {
        format!("env key '{key}'")
    } else {
        "an env key".to_owned()
    }
}
