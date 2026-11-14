//! `pohunek plugin profile fold`: proposes how a family of host profiles
//! collapses into one parent profile with `[options]` and one preset per
//! input.
//!
//! The command only reads: it loads the named files with the owner-secure
//! loader of the other profile commands and prints the proposal, so
//! `--dry-run` is required. The inputs must agree on every launch key but
//! `args` and `[env]`. Each `args` list is split into flag groups; a group
//! equal in every input stays a shared argument, any other group becomes an
//! option whose choices are the inputs' variants. An `[env]` key with one
//! value everywhere stays shared, any other becomes an `env-<key>` option.
//!
//! Env values never leave memory: the proposal holds the placeholder
//! `"<value of KEY from PROFILE>"` for each one. Every argument the proposal
//! prints passes the credential detector the daemon applies, and errors name
//! keys, profiles and argument indexes only.

// Rust guideline compliant 2026-11-14

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use clap::Args;
use package::credential::credential_arg_problem;
use serde::Serialize;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value};

use super::plugin::{print_output, Error};
use super::plugin_profile::{
    directory_error, is_valid_profile_name, load_profile, open_agents_dir, parse_profile_name,
    AGENTS_DIR,
};
use crate::error::CliError;
use crate::paths::Paths;

/// Root keys every input must hold with one value: the launch keys other
/// than `args` and `[env]`.
const EQUAL_KEYS: [&str; 8] = [
    "base",
    "package",
    "digest",
    "program",
    "manifest",
    "resume",
    "input_rules",
    "secret_env",
];

/// Root keys a full profile file may hold.
const PROFILE_KEYS: [&str; 14] = [
    "base",
    "package",
    "digest",
    "program",
    "args",
    "env",
    "secret_env",
    "input_rules",
    "resume",
    "manifest",
    "display_name",
    "audience",
    "options",
    "preset",
];

/// Group key of the arguments before the first flag.
const POSITIONAL_GROUP: &str = "_positional";

/// Choice id of an option whose key an input does not set.
const NONE_CHOICE: &str = "none";

/// Prefix of the option id that varies an `[env]` key.
const ENV_OPTION_PREFIX: &str = "env-";

/// Most bytes of an option or choice id, the daemon's
/// `MAX_PROFILE_OPTION_ID_BYTES`.
const MAX_ID_BYTES: usize = protocol::MAX_PROFILE_OPTION_ID_BYTES;

/// Env prefix the daemon reserves for its handshake and strips from `[env]`.
const RESERVED_ENV_PREFIX: &str = "POHUNEK_";

/// Arguments of `pohunek plugin profile fold`.
#[derive(Debug, Clone, Args)]
pub(crate) struct FoldArgs {
    /// Profiles to fold, two or more: file names under `agents/` without
    /// `.toml`.
    #[arg(
        value_name = "NAME",
        value_parser = parse_profile_name,
        required = true,
        num_args = 2..
    )]
    name: Vec<String>,
    /// Name of the proposed parent profile; not one of the inputs.
    #[arg(long, value_name = "PARENT", value_parser = parse_profile_name)]
    into: String,
    /// Print the proposal and write nothing. Required: fold never writes.
    #[arg(long, required = true)]
    dry_run: bool,
    /// Emit machine-readable JSON instead of text.
    #[arg(long)]
    json: bool,
}

impl FoldArgs {
    /// Whether the proposal prints as JSON.
    pub(crate) fn json(&self) -> bool {
        self.json
    }
}

/// The proposal `fold` prints.
#[derive(Debug, Serialize)]
struct Proposal {
    /// The parent profile.
    parent: ProposedFile,
    /// One preset per input, in input order.
    presets: Vec<ProposedPreset>,
    /// Old and new argument lists of each reordered input, for the text
    /// output only.
    #[serde(skip)]
    reorders: Vec<Option<(String, String)>>,
}

/// A proposed file: its profile name and text.
#[derive(Debug, Serialize)]
struct ProposedFile {
    name: String,
    text: String,
}

/// A proposed preset with the argument check of its input.
#[derive(Debug, Serialize)]
struct ProposedPreset {
    name: String,
    text: String,
    /// `equal` when the parent with the preset's choices launches the
    /// input's arguments in the same order, else `reordered`.
    args_order: ArgsOrder,
}

/// Whether a preset launches its input's arguments in their order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ArgsOrder {
    Equal,
    Reordered,
}

/// One input profile, parsed. Holds env values: no `Debug`, never printed.
struct Input {
    name: String,
    root: toml::Table,
    args: Vec<String>,
    groups: Vec<Group>,
    env: BTreeMap<String, String>,
}

/// One flag group of an `args` list: the flag key and its tokens.
#[derive(Clone, PartialEq, Eq)]
struct Group {
    key: String,
    tokens: Vec<String>,
}

/// An option of the proposed parent.
struct ProposedOption {
    /// Choice ids with their args or env entry, in choice order.
    choices: Vec<(String, ChoiceBody)>,
    /// The choice each input takes, by input index.
    taken: Vec<String>,
}

/// Proposed options with the args group key or env key each one varies.
type KeyedOptions = Vec<(String, ProposedOption)>;

/// What a proposed choice adds to the launch.
enum ChoiceBody {
    /// Argument tokens.
    Args(Vec<String>),
    /// One env key with its placeholder value.
    Env(String, String),
    /// Nothing: the input does not set the key.
    Nothing,
}

/// Runs `fold` against the local agents directory.
///
/// # Errors
///
/// [`Error::ProfileDirectory`] for an unusable agents directory,
/// [`Error::FoldInput`] for an input that cannot be read or parsed, and the
/// fold refusals of [`fold`].
pub(crate) fn run(args: &FoldArgs) -> Result<(), CliError> {
    let paths = Paths::resolve()?;
    let dir = open_agents_dir(&paths.config_dir.join(AGENTS_DIR))
        .map_err(|error| directory_error(&error))?;
    let mut seen = BTreeSet::new();
    let mut texts = Vec::with_capacity(args.name.len());
    for name in &args.name {
        if !seen.insert(name.as_str()) {
            return Err(fold_input(name, "is named twice").into());
        }
        if *name == args.into {
            return Err(fold_input(
                name,
                "is the parent name; the parent needs a name of its own",
            )
            .into());
        }
        let Some(dir) = dir.as_ref() else {
            return Err(fold_input(name, "the agents directory does not exist").into());
        };
        let loaded = load_profile(dir, name).map_err(|fault| fold_input(name, fault.detail()))?;
        texts.push((name.clone(), loaded.text));
    }
    let proposal = fold(texts, &args.into)?;
    print_output(args.json, &proposal, || render(&proposal))
}

/// [`Error::FoldInput`] for the input `name`.
fn fold_input(name: &str, detail: &str) -> Error {
    Error::FoldInput {
        name: name.to_owned(),
        detail: detail.to_owned(),
    }
}

/// Folds the input profiles `texts` (name, file text) into a parent named
/// `parent` and one preset per input.
///
/// # Errors
///
/// [`Error::FoldInput`] for an input that is not a full profile file,
/// [`Error::FoldIncompatible`] for a preset, a profile with `[options]` or
/// inputs that differ in a launch key, [`Error::FoldAmbiguousArgs`] for a
/// flag an input repeats, and [`Error::FoldCredentialLiteral`] for an
/// argument that looks like a credential.
fn fold(texts: Vec<(String, String)>, parent: &str) -> Result<Proposal, Error> {
    let inputs = texts
        .into_iter()
        .map(|(name, text)| parse_input(name, &text))
        .collect::<Result<Vec<_>, _>>()?;
    check_equal(&inputs)?;
    let (shared_args, arg_options) = fold_args(&inputs);
    let (shared_env, env_options) = fold_env(&inputs);
    let mut used = BTreeSet::new();
    let mut options = BTreeMap::new();
    for (key, option) in arg_options {
        let base = sanitized_id(key.trim_start_matches('-')).unwrap_or_else(|| "o".to_owned());
        options.insert(unique_id(&base, &mut used), option);
    }
    for (key, option) in env_options {
        let base = sanitized_id(&format!("{ENV_OPTION_PREFIX}{}", key.to_ascii_lowercase()))
            .unwrap_or_else(|| "env".to_owned());
        options.insert(unique_id(&base, &mut used), option);
    }
    let first = &inputs[0];
    let parent_text = parent_text(first, &shared_args, &shared_env, &options);
    let mut presets = Vec::with_capacity(inputs.len());
    let mut reorders = Vec::with_capacity(inputs.len());
    for (index, input) in inputs.iter().enumerate() {
        let mut launched = shared_args.clone();
        for option in options.values() {
            let taken = &option.taken[index];
            if let Some((_, ChoiceBody::Args(tokens))) =
                option.choices.iter().find(|(id, _)| id == taken)
            {
                launched.extend(tokens.iter().cloned());
            }
        }
        let args_order = if launched == input.args {
            reorders.push(None);
            ArgsOrder::Equal
        } else {
            reorders.push(Some((input.args.join(" "), launched.join(" "))));
            ArgsOrder::Reordered
        };
        presets.push(ProposedPreset {
            name: input.name.clone(),
            text: preset_text(input, parent, index, &options),
            args_order,
        });
    }
    Ok(Proposal {
        parent: ProposedFile {
            name: parent.to_owned(),
            text: parent_text,
        },
        presets,
        reorders,
    })
}

/// Parses the input `name` with file text `text`.
fn parse_input(name: String, text: &str) -> Result<Input, Error> {
    // The parser's own message quotes the offending line, which can be an
    // `[env]` value; the error names the file only.
    let mut root: toml::Table =
        toml::from_str(text).map_err(|_error| fold_input(&name, "the file is not valid TOML"))?;
    // The daemon reads the legacy `fork_args` and drops it, so it is no
    // launch input: it is neither compared nor copied, and its tokens, which
    // the credential check does not cover, are never printed.
    if let Some(toml::Value::Table(resume)) = root.get_mut("resume") {
        resume.remove("fork_args");
    }
    if root.contains_key("preset") {
        return Err(Error::FoldIncompatible {
            detail: format!("profile {name} is a preset; fold takes full profiles"),
        });
    }
    if root.contains_key("options") {
        return Err(Error::FoldIncompatible {
            detail: format!("profile {name} declares [options]; fold takes profiles without them"),
        });
    }
    if root.keys().any(|key| !PROFILE_KEYS.contains(&key.as_str())) {
        return Err(fold_input(&name, "holds a key a profile does not have"));
    }
    if !root.get("base").is_some_and(toml::Value::is_str) {
        return Err(fold_input(&name, "has no base"));
    }
    let args = strings(root.get("args"))
        .ok_or_else(|| fold_input(&name, "args is not a list of strings"))?;
    if let Some(index) = credential_arg_problem(&args) {
        return Err(Error::FoldCredentialLiteral {
            profile: name,
            field: "args",
            index,
        });
    }
    let resume_args = root
        .get("resume")
        .and_then(toml::Value::as_table)
        .and_then(|resume| resume.get("args"));
    if let Some(index) = strings(resume_args)
        .as_deref()
        .and_then(credential_arg_problem)
    {
        return Err(Error::FoldCredentialLiteral {
            profile: name,
            field: "resume.args",
            index,
        });
    }
    let env = match root.get("env") {
        None => BTreeMap::new(),
        Some(toml::Value::Table(table)) => table
            .iter()
            .filter(|(key, _)| !key.starts_with(RESERVED_ENV_PREFIX))
            .map(|(key, value)| value.as_str().map(|text| (key.clone(), text.to_owned())))
            .collect::<Option<_>>()
            .ok_or_else(|| fold_input(&name, "[env] holds a value that is not a string"))?,
        Some(_) => return Err(fold_input(&name, "env is not a table")),
    };
    let groups = groups(&name, &args)?;
    Ok(Input {
        name,
        root,
        args,
        groups,
        env,
    })
}

/// The strings of the array `value`; an absent value is an empty list and
/// any other shape is `None`.
fn strings(value: Option<&toml::Value>) -> Option<Vec<String>> {
    match value {
        None => Some(Vec::new()),
        Some(toml::Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect(),
        Some(_) => None,
    }
}

/// Splits `args` of profile `name` into flag groups: a group starts at a
/// token beginning with `-` and takes the tokens up to the next one; tokens
/// before the first flag form [`POSITIONAL_GROUP`]. The key of a group is
/// its flag up to `=`.
fn groups(name: &str, args: &[String]) -> Result<Vec<Group>, Error> {
    let mut groups: Vec<Group> = Vec::new();
    let mut keys = BTreeSet::new();
    for (index, token) in args.iter().enumerate() {
        if token.starts_with('-') {
            let key = token
                .split_once('=')
                .map_or(token.as_str(), |(flag, _)| flag);
            if !keys.insert(key.to_owned()) {
                return Err(Error::FoldAmbiguousArgs {
                    profile: name.to_owned(),
                    index,
                });
            }
            groups.push(Group {
                key: key.to_owned(),
                tokens: vec![token.clone()],
            });
        } else if let Some(group) = groups.last_mut() {
            group.tokens.push(token.clone());
        } else {
            keys.insert(POSITIONAL_GROUP.to_owned());
            groups.push(Group {
                key: POSITIONAL_GROUP.to_owned(),
                tokens: vec![token.clone()],
            });
        }
    }
    Ok(groups)
}

/// Refuses inputs that differ in any of [`EQUAL_KEYS`], naming the key and
/// two of the profiles, never a value.
fn check_equal(inputs: &[Input]) -> Result<(), Error> {
    let first = &inputs[0];
    for key in EQUAL_KEYS {
        if let Some(other) = inputs
            .iter()
            .skip(1)
            .find(|input| input.root.get(key) != first.root.get(key))
        {
            return Err(Error::FoldIncompatible {
                detail: format!(
                    "profiles {} and {} differ in `{key}`; fold needs it equal in every input",
                    first.name, other.name
                ),
            });
        }
    }
    Ok(())
}

/// The shared args (in the first input's order) and one option per group
/// key that differs between the inputs, by group key in order of first
/// appearance.
fn fold_args(inputs: &[Input]) -> (Vec<String>, KeyedOptions) {
    let mut keys: Vec<&str> = Vec::new();
    for group in inputs.iter().flat_map(|input| &input.groups) {
        if !keys.contains(&group.key.as_str()) {
            keys.push(&group.key);
        }
    }
    let variant = |input: &Input, key: &str| -> Option<Group> {
        input.groups.iter().find(|group| group.key == key).cloned()
    };
    let mut shared_keys = BTreeSet::new();
    let mut options = Vec::new();
    for key in keys {
        let variants: Vec<Option<Group>> = inputs.iter().map(|input| variant(input, key)).collect();
        let shared = variants[0].is_some() && variants.iter().all(|other| *other == variants[0]);
        if shared {
            shared_keys.insert(key.to_owned());
            continue;
        }
        let values: Vec<Option<Vec<String>>> = variants
            .into_iter()
            .map(|group| group.map(|group| group.tokens))
            .collect();
        options.push((
            key.to_owned(),
            proposed_option(&values, |tokens, number| {
                let id = sanitized_id(&choice_words(key, tokens).join("-"))
                    .unwrap_or_else(|| format!("c{number}"));
                (id, ChoiceBody::Args(tokens.to_vec()))
            }),
        ));
    }
    let shared = inputs[0]
        .groups
        .iter()
        .filter(|group| shared_keys.contains(&group.key))
        .flat_map(|group| group.tokens.iter().cloned())
        .collect();
    (shared, options)
}

/// The words a choice id is made of: the value tokens of a flag group (the
/// tokens after the flag and the part of the flag after `=`), or every token
/// of the positional group.
fn choice_words<'a>(key: &str, tokens: &'a [String]) -> Vec<&'a str> {
    if key == POSITIONAL_GROUP {
        return tokens.iter().map(String::as_str).collect();
    }
    let mut words = Vec::with_capacity(tokens.len());
    if let Some((_, value)) = tokens.first().and_then(|flag| flag.split_once('=')) {
        words.push(value);
    }
    words.extend(tokens.iter().skip(1).map(String::as_str));
    words
}

/// The shared `[env]` keys (with the first input's placeholder) and one
/// option per key whose value differs or is missing somewhere, by key.
fn fold_env(inputs: &[Input]) -> (Vec<(String, String)>, KeyedOptions) {
    let keys: BTreeSet<&String> = inputs.iter().flat_map(|input| input.env.keys()).collect();
    let mut shared = Vec::new();
    let mut options = Vec::new();
    for key in keys {
        let values: Vec<Option<&String>> = inputs.iter().map(|input| input.env.get(key)).collect();
        if values[0].is_some() && values.iter().all(|value| *value == values[0]) {
            shared.push((key.clone(), placeholder(key, &inputs[0].name)));
            continue;
        }
        // The choices compare the values in memory; a choice carries the
        // placeholder naming the first input that holds its value.
        let mut first_owner: BTreeMap<&String, &str> = BTreeMap::new();
        for (value, input) in values.iter().zip(inputs) {
            if let Some(value) = value {
                first_owner.entry(*value).or_insert(input.name.as_str());
            }
        }
        let keyed: Vec<Option<Vec<String>>> = values
            .iter()
            .map(|value| value.map(|value| vec![value.clone()]))
            .collect();
        options.push((
            key.clone(),
            proposed_option(&keyed, |value, number| {
                let owner = first_owner.get(&value[0]).copied().unwrap_or_default();
                (
                    format!("v{number}"),
                    ChoiceBody::Env(key.clone(), placeholder(key, owner)),
                )
            }),
        ));
    }
    (shared, options)
}

/// The placeholder that stands for the value of env `key` from `profile`.
fn placeholder(key: &str, profile: &str) -> String {
    format!("<value of {key} from {profile}>")
}

/// An option over the per-input `values` (`None` where an input does not
/// set the key): one choice per distinct value in input order, made by
/// `choice` from the value and its 1-based number, plus `none` when an
/// input lacks the key. Choice ids are made unique.
fn proposed_option(
    values: &[Option<Vec<String>>],
    choice: impl Fn(&[String], usize) -> (String, ChoiceBody),
) -> ProposedOption {
    let mut used = BTreeSet::new();
    let mut choices: Vec<(String, ChoiceBody)> = Vec::new();
    let mut by_value: Vec<(&Vec<String>, String)> = Vec::new();
    let mut none_id = None;
    let mut taken = Vec::with_capacity(values.len());
    for value in values {
        let id = match value {
            Some(value) => {
                if let Some((_, id)) = by_value.iter().find(|(seen, _)| *seen == value) {
                    id.clone()
                } else {
                    let (base, body) = choice(value, by_value.len() + 1);
                    let id = unique_id(&base, &mut used);
                    by_value.push((value, id.clone()));
                    choices.push((id.clone(), body));
                    id
                }
            }
            None => none_id
                .get_or_insert_with(|| {
                    let id = unique_id(NONE_CHOICE, &mut used);
                    choices.push((id.clone(), ChoiceBody::Nothing));
                    id
                })
                .clone(),
        };
        taken.push(id);
    }
    ProposedOption { choices, taken }
}

/// `text` as an option or choice id: letters, digits, `.`, `_` and `-`
/// (anything else becomes `-`), no leading `.` or `-`, no `..`, at most
/// [`MAX_ID_BYTES`] bytes. `None` when nothing is left.
fn sanitized_id(text: &str) -> Option<String> {
    let mapped: String = text
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '-'
            }
        })
        .collect();
    let mut id = mapped.replace("..", "-");
    while id.contains("..") {
        id = id.replace("..", "-");
    }
    let id = id.trim_start_matches(['.', '-']);
    let mut end = id.len().min(MAX_ID_BYTES);
    while !id.is_char_boundary(end) {
        end -= 1;
    }
    let id = &id[..end];
    (!id.is_empty() && is_valid_profile_name(id)).then(|| id.to_owned())
}

/// `base`, or `base-<n>` for the first free `n`, recorded in `used`; the
/// result stays within [`MAX_ID_BYTES`].
fn unique_id(base: &str, used: &mut BTreeSet<String>) -> String {
    let mut candidate = base.to_owned();
    let mut number = 2_usize;
    while used.contains(&candidate) {
        let suffix = format!("-{number}");
        let keep = base.len().min(MAX_ID_BYTES.saturating_sub(suffix.len()));
        candidate = format!("{}{suffix}", &base[..keep]);
        number += 1;
    }
    used.insert(candidate.clone());
    candidate
}

/// The parent profile text: the first input's launch keys, the shared args
/// and env placeholders, and the options.
fn parent_text(
    first: &Input,
    shared_args: &[String],
    shared_env: &[(String, String)],
    options: &BTreeMap<String, ProposedOption>,
) -> String {
    let mut document = DocumentMut::new();
    let root = document.as_table_mut();
    for key in ["base", "package", "digest", "program"] {
        if let Some(value) = first.root.get(key) {
            root.insert(key, Item::Value(edit_value(value)));
        }
    }
    if !shared_args.is_empty() {
        root.insert("args", Item::Value(Value::Array(string_array(shared_args))));
    }
    for key in ["secret_env", "manifest"] {
        if let Some(value) = first.root.get(key) {
            root.insert(key, Item::Value(edit_value(value)));
        }
    }
    for key in ["input_rules", "resume"] {
        if let Some(toml::Value::Table(table)) = first.root.get(key) {
            root.insert(key, Item::Table(edit_table(table)));
        }
    }
    if !shared_env.is_empty() {
        let mut env = Table::new();
        for (key, value) in shared_env {
            env.insert(key, Item::Value(Value::from(value.as_str())));
        }
        root.insert("env", Item::Table(env));
    }
    if !options.is_empty() {
        let mut table = Table::new();
        table.set_implicit(true);
        for (id, option) in options {
            table.insert(
                id,
                Item::Table(option_table(id, option, &first_choice(option))),
            );
        }
        root.insert("options", Item::Table(table));
    }
    document.to_string()
}

/// The choice the first input takes: the option's default.
fn first_choice(option: &ProposedOption) -> String {
    option.taken[0].clone()
}

/// The `[options.<id>]` table of `option` with `default`.
fn option_table(id: &str, option: &ProposedOption, default: &str) -> Table {
    let mut table = Table::new();
    table.insert("label", Item::Value(Value::from(id)));
    table.insert("default", Item::Value(Value::from(default)));
    let mut choices = Table::new();
    choices.set_dotted(true);
    for (choice, body) in &option.choices {
        let mut inline = InlineTable::new();
        inline.insert("label", Value::from(choice.as_str()));
        match body {
            ChoiceBody::Args(tokens) => {
                inline.insert("args", Value::Array(string_array(tokens)));
            }
            ChoiceBody::Env(key, value) => {
                let mut env = InlineTable::new();
                env.insert(key, Value::from(value.as_str()));
                inline.insert("env", Value::InlineTable(env));
            }
            ChoiceBody::Nothing => {}
        }
        choices.insert(choice, Item::Value(Value::InlineTable(inline)));
    }
    table.insert("choices", Item::Table(choices));
    table
}

/// The preset text of input number `index`: its labels, the parent and the
/// choice it takes for every option.
fn preset_text(
    input: &Input,
    parent: &str,
    index: usize,
    options: &BTreeMap<String, ProposedOption>,
) -> String {
    let mut document = DocumentMut::new();
    let root = document.as_table_mut();
    for key in ["display_name", "audience"] {
        if let Some(value) = input.root.get(key) {
            root.insert(key, Item::Value(edit_value(value)));
        }
    }
    let mut preset = Table::new();
    preset.insert("parent", Item::Value(Value::from(parent)));
    if !options.is_empty() {
        let mut chosen = Table::new();
        for (id, option) in options {
            chosen.insert(id, Item::Value(Value::from(option.taken[index].as_str())));
        }
        preset.insert("options", Item::Table(chosen));
    }
    root.insert("preset", Item::Table(preset));
    document.to_string()
}

/// `items` as a TOML array of strings.
fn string_array(items: &[String]) -> Array {
    items.iter().map(String::as_str).collect()
}

/// `value` as an editable TOML value; a table becomes an inline table.
fn edit_value(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(text) => Value::from(text.as_str()),
        toml::Value::Integer(number) => Value::from(*number),
        toml::Value::Float(number) => Value::from(*number),
        toml::Value::Boolean(flag) => Value::from(*flag),
        toml::Value::Datetime(datetime) => Value::from(*datetime),
        toml::Value::Array(items) => Value::Array(items.iter().map(edit_value).collect()),
        toml::Value::Table(table) => {
            let mut inline = InlineTable::new();
            for (key, item) in table {
                inline.insert(key, edit_value(item));
            }
            Value::InlineTable(inline)
        }
    }
}

/// `table` as an editable TOML table.
fn edit_table(table: &toml::Table) -> Table {
    let mut edited = Table::new();
    for (key, value) in table {
        edited.insert(key, Item::Value(edit_value(value)));
    }
    edited
}

/// The text form of `proposal`.
fn render(proposal: &Proposal) -> String {
    let mut text = format!(
        "Proposed parent profile {} (agents/{}.toml):\n\n{}\n",
        proposal.parent.name, proposal.parent.name, proposal.parent.text
    );
    for preset in &proposal.presets {
        let _ = write!(
            text,
            "Preset {} (replaces agents/{}.toml):\n\n{}\n",
            preset.name, preset.name, preset.text
        );
    }
    text.push_str("Checks:\n");
    for (preset, reorder) in proposal.presets.iter().zip(&proposal.reorders) {
        let _ = match reorder {
            None => writeln!(text, "  {}: args equal", preset.name),
            Some((old, new)) => {
                writeln!(text, "  {}: args reordered: {old} -> {new}", preset.name)
            }
        };
    }
    text.push_str(
        "\nNothing was written. Each `<value of KEY from PROFILE>` stands for that profile's \
         value; keep an old profile file until its sessions are gone, because a preset's \
         revision differs from the full profile's.\n",
    );
    text
}
