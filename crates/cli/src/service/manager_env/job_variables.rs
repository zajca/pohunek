//! The variables a daemon job gets besides its `PATH`.
//!
//! The daemon unit file the install writes sets the bootstrap keys (`HOME`,
//! `XDG_*`, `PATH`), and a drop-in applies after it: its `Environment=` lines
//! come after the unit file's, and `UnsetEnvironment=` removes last. The store
//! variables an allowlist names (`CLAUDE_CONFIG_DIR`, ...) come from the user
//! manager's environment, which the unit's `Environment=` replaces. A judgement
//! made with other values than the job gets can pass what loses a conversation
//! or fail one that is safe, so a drop-in's effect is applied for a store
//! variable and refused for a bootstrap key. The drop-in files are read with the
//! reader of a removed unit's drop-ins, which is also what a recovery that
//! removes the unit sees. Values are never part of an error.

// Rust guideline compliant 2026-10-14

use std::collections::BTreeMap;

use pohunek_platform::supervisor::{
    UnitEnvironment, BOOTSTRAP_ENV_ALLOWLIST, SEARCH_PATH_VARIABLE,
};

use super::{
    allowlisted_variables, drop_in_files_of, drop_ins_by_priority, environment_file_keys,
    names_variable, read_drop_ins, refuse_unfollowed_syntax, service_lines,
    split_environment_words, split_words, unit_state_check, without_specifiers, UnitFileReader,
    SYSTEMD_WHITESPACE,
};
use crate::service::error::Error;

/// An `Environment=` or filter line whose effect is not read here.
struct UnclearLine {
    file: String,
    text: String,
    /// An escape ahead of the first `=` of a word, which can spell a name.
    name_escaped: bool,
    /// Bytes that are not UTF-8: the line is judged by its lossy text.
    lossy: bool,
}

impl UnclearLine {
    /// Whether the line may set or remove a variable that `pred` accepts.
    fn mentions(&self, pred: impl Fn(&str) -> bool) -> bool {
        if self.name_escaped || (self.lossy && self.text.contains("Environment")) {
            return true;
        }
        // An escape sequence ends a word and is not part of the next one
        // (`\x20NAME`, `\040NAME`, `\nNAME`), so the code after each backslash
        // is dropped before the text is split into names.
        let mut segments = self.text.split('\\');
        let first = segments.next().unwrap_or_default();
        std::iter::once(first)
            .chain(segments.map(strip_escape_code))
            .flat_map(|segment| segment.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')))
            .any(|token| !token.is_empty() && pred(token))
    }
}

/// `segment`, which follows a backslash, without the escape code at its start:
/// `xHH`, three octal digits, or one letter.
fn strip_escape_code(segment: &str) -> &str {
    let bytes = segment.as_bytes();
    let hex = bytes.first() == Some(&b'x')
        && bytes.len() >= 3
        && bytes[1].is_ascii_hexdigit()
        && bytes[2].is_ascii_hexdigit();
    let octal = bytes.len() >= 3 && bytes[..3].iter().all(|b| (b'0'..=b'7').contains(b));
    let code = if hex || octal {
        3
    } else {
        usize::from(!bytes.is_empty())
    };
    &segment[code..]
}

/// What the enabled drop-ins of a unit say about its environment. It has no
/// `Debug`: the assigned values may be secrets.
#[derive(Default)]
struct DropInEnvironment {
    /// The plain assignments in force: name to the drop-in and the raw value.
    assigned: BTreeMap<String, (String, String)>,
    /// The first drop-in that resets `Environment=`.
    reset: Option<String>,
    /// `Environment=` lines not read here.
    unclear: Vec<UnclearLine>,
    /// The `UnsetEnvironment=` names in force, with the value they require.
    unset: Vec<(String, Option<String>)>,
    /// `PassEnvironment=` names in force.
    pass: Vec<String>,
    /// `PassEnvironment=` and `UnsetEnvironment=` lines not read here.
    unclear_filters: Vec<UnclearLine>,
}

async fn scan_drop_ins(
    unit: &UnitEnvironment,
    unit_name: &str,
    reader: &mut UnitFileReader,
) -> Result<DropInEnvironment, Error> {
    let mut facts = DropInEnvironment::default();
    for path in drop_ins_by_priority(&drop_in_files_of(unit), unit_name)? {
        let Some(bytes) = reader.drop_in(&path, unit_name).await? else {
            continue;
        };
        refuse_unfollowed_syntax(&bytes, &path, unit_name)?;
        for line in service_lines(&bytes) {
            let line = match line {
                Ok(line) => line,
                Err(text) => {
                    facts.unclear.push(UnclearLine {
                        file: path.clone(),
                        text,
                        name_escaped: false,
                        lossy: true,
                    });
                    continue;
                }
            };
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let empty = value.trim_matches(SYSTEMD_WHITESPACE).is_empty();
            match key.trim_end_matches(SYSTEMD_WHITESPACE) {
                "Environment" if empty => {
                    facts.assigned.clear();
                    facts.reset.get_or_insert_with(|| path.clone());
                }
                "Environment" => {
                    let split = split_environment_words(value);
                    if split.open_quote || split.escaped {
                        facts.unclear.push(UnclearLine {
                            file: path.clone(),
                            text: value.to_owned(),
                            name_escaped: split.name_escaped,
                            lossy: false,
                        });
                        continue;
                    }
                    for word in split.words {
                        let Some((name, assigned)) = word.split_once('=') else {
                            continue;
                        };
                        if without_specifiers(name).is_err() {
                            // A specifier in a name can spell any variable.
                            facts.unclear.push(UnclearLine {
                                file: path.clone(),
                                text: value.to_owned(),
                                name_escaped: true,
                                lossy: false,
                            });
                            continue;
                        }
                        facts
                            .assigned
                            .insert(name.to_owned(), (path.clone(), assigned.to_owned()));
                    }
                }
                directive @ ("UnsetEnvironment" | "PassEnvironment") if empty => {
                    if directive == "UnsetEnvironment" {
                        facts.unset.clear();
                    } else {
                        facts.pass.clear();
                    }
                }
                directive @ ("UnsetEnvironment" | "PassEnvironment") => {
                    let split = split_words(value);
                    if split.open_quote || split.escaped || value.contains('%') {
                        facts.unclear_filters.push(UnclearLine {
                            file: path.clone(),
                            text: value.to_owned(),
                            name_escaped: true,
                            lossy: false,
                        });
                        continue;
                    }
                    for word in split.words {
                        if directive == "UnsetEnvironment" {
                            facts.unset.push(match word.split_once('=') {
                                Some((name, wanted)) => (name.to_owned(), Some(wanted.to_owned())),
                                None => (word, None),
                            });
                        } else {
                            facts.pass.push(word);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(facts)
}

fn refuse(detail: String) -> Error {
    Error::ManagerEnvironment { detail }
}

/// The value of a drop-in assignment: its specifiers are not expanded here.
fn plain_value(name: &str, file: &str, raw: &str, unit_name: &str) -> Result<String, Error> {
    without_specifiers(raw).map_err(|specifier| {
        refuse(format!(
            "{unit_name}: the drop-in {file} uses the specifier {specifier} in the value of {name}, which is not expanded here: a recovery that removes the unit resolves its drop-ins without the service manager. Write the value out and retry"
        ))
    })
}

/// What one judgement knows about the daemon unit's settings.
struct Judgement<'a> {
    unit: &'a UnitEnvironment,
    unit_name: &'a str,
    allowlist: &'a [String],
    facts: DropInEnvironment,
    /// The names the `EnvironmentFile=` files assign, with the file.
    file_keys: Vec<(String, String)>,
    loaded: bool,
}

impl Judgement<'_> {
    /// Whether the allowlist selects `name` as a store variable: the job's
    /// `PATH` and the bootstrap keys have their own rules.
    fn selects(&self, name: &str) -> bool {
        name != SEARCH_PATH_VARIABLE
            && !BOOTSTRAP_ENV_ALLOWLIST.contains(&name)
            && allowlisted_variables([(name.to_owned(), String::new())], self.allowlist)
                .is_ok_and(|selected| !selected.is_empty())
    }

    /// Refuses what sets or removes a key of the bootstrap environment the unit
    /// file gives the job.
    fn refuse_bootstrap_overrides(&self) -> Result<(), Error> {
        let Self {
            unit,
            unit_name,
            facts,
            file_keys,
            ..
        } = self;
        if let Some(file) = &facts.reset {
            return Err(refuse(format!(
                "{unit_name}: the drop-in {file} resets Environment=, which removes the bootstrap environment (HOME, XDG_*) that the unit file the install writes gives its job. Remove the empty `Environment=` line and retry"
            )));
        }
        for key in BOOTSTRAP_ENV_ALLOWLIST {
            let named = |token: &str| token == key;
            let place = if let Some((file, _)) = facts.assigned.get(key) {
                Some(format!("the drop-in {file} sets {key}"))
            } else if let Some(line) = facts
                .unclear
                .iter()
                .chain(&facts.unclear_filters)
                .find(|line| line.mentions(named))
            {
                Some(format!(
                    "the drop-in {} has an Environment=, PassEnvironment= or UnsetEnvironment= line that may set or remove {key} in a form not read here",
                    line.file
                ))
            } else if facts.unset.iter().any(|(name, _)| name == key)
                || unit
                    .unset_environment
                    .iter()
                    .any(|name| names_variable(name, key))
            {
                Some(format!("the unit sets UnsetEnvironment= for {key}"))
            } else {
                file_keys
                    .iter()
                    .find(|(name, _)| name == key)
                    .map(|(_, file)| format!("the EnvironmentFile= {file} assigns {key}"))
            };
            if let Some(place) = place {
                return Err(refuse(format!(
                    "{unit_name}: {place}, but the unit file the install writes gives the daemon job its {key}, so the preflight would judge another {key} than its job gets. Remove it (`systemctl --user cat {unit_name}`) and retry"
                )));
            }
        }
        Ok(())
    }

    /// Refuses what may change a store variable in a way that is not derived
    /// here: an `EnvironmentFile=` assignment, a `PassEnvironment=` that no
    /// drop-in assignment overrides, and a drop-in line that is not read.
    fn refuse_unreadable_store_settings(&self) -> Result<(), Error> {
        let Self {
            unit_name,
            facts,
            file_keys,
            allowlist,
            ..
        } = self;
        if allowlist.is_empty() {
            // Nothing is selected, so no store variable is judged.
            return Ok(());
        }
        if let Some((name, file)) = file_keys.iter().find(|(name, _)| self.selects(name)) {
            return Err(refuse(format!(
                "{unit_name}: the EnvironmentFile= {file} assigns {name}, which the daemon reads from its environment, and the value it gets is not re-parsed here. Move the assignment to an Environment= line of a drop-in and retry"
            )));
        }
        for name in &facts.pass {
            if self.selects(name) && !facts.assigned.contains_key(name) {
                return Err(refuse(format!(
                    "{unit_name} sets PassEnvironment= with {name}, whose value is the service manager process's own and cannot be read here. Remove {name} from PassEnvironment= or set it in an Environment= line of a drop-in, and retry"
                )));
            }
        }
        // A line that may touch a selected name is refused whether or not the
        // name is set now: once a rollback removes the unit the manager's list
        // no longer says which names the line sets, and the same files must
        // give the same answer then.
        let touches = |token: &str| self.selects(token);
        if let Some(line) = facts
            .unclear
            .iter()
            .chain(&facts.unclear_filters)
            .find(|line| line.mentions(touches))
        {
            return Err(refuse(format!(
                "{unit_name}: the drop-in {} has an Environment=, PassEnvironment= or UnsetEnvironment= line that may set or remove a variable the daemon reads from its environment, in a form not read here (an escape sequence, a quote never closed, a specifier, or bytes that are not UTF-8). Write a plain assignment and retry",
                line.file
            )));
        }
        Ok(())
    }

    /// Applies the drop-ins' assignments and removals to `manager`.
    fn apply(&self, manager: &mut BTreeMap<String, String>) -> Result<(), Error> {
        for (name, (file, raw)) in &self.facts.assigned {
            if !self.selects(name) {
                continue;
            }
            let value = plain_value(name, file, raw, self.unit_name)?;
            if self.loaded {
                let prefix = format!("{name}=");
                let merged = self
                    .unit
                    .environment
                    .iter()
                    .rev()
                    .find_map(|entry| entry.strip_prefix(prefix.as_str()));
                if merged != Some(value.as_str()) {
                    return Err(refuse(format!(
                        "{}: the {name} that the service manager reports for the unit differs from the one the drop-in {file} sets, so what its job gets is not derived here. Write the value as a plain assignment and retry",
                        self.unit_name
                    )));
                }
            }
            manager.insert(name.clone(), value);
        }
        for (name, wanted) in &self.facts.unset {
            if !self.selects(name) {
                continue;
            }
            let remove = match wanted {
                None => true,
                Some(wanted) => manager.get(name).is_some_and(|value| value == wanted),
            };
            if remove {
                manager.remove(name);
            }
        }
        Ok(())
    }
}

/// The variables of the daemon job: the allowlisted ones, with the unit's
/// drop-ins applied over the user manager's `entries`, after the bootstrap keys
/// are checked against the unit's drop-ins.
///
/// # Errors
///
/// Returns [`Error::ManagerEnvironment`] when a drop-in or an `EnvironmentFile=`
/// sets or removes a bootstrap key, an `EnvironmentFile=` assigns a variable the
/// allowlist selects, `PassEnvironment=` names one that no drop-in assigns, a
/// drop-in line that may set or remove one is not read here, or a value cannot be
/// carried.
pub async fn unit_job_variables(
    entries: &[String],
    unit: &UnitEnvironment,
    unit_name: &str,
    allowlist: &[String],
    reader: &mut UnitFileReader,
) -> Result<BTreeMap<String, String>, Error> {
    let mut manager: BTreeMap<String, String> = entries
        .iter()
        .filter_map(|entry| entry.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    let has_drop_ins = !unit.drop_in_paths.is_empty() || !unit.drop_ins_on_disk.is_empty();
    if !has_drop_ins && unit.load_state == "not-found" {
        return allowlisted_variables(manager, allowlist);
    }
    unit_state_check(unit, unit_name)?;
    if !has_drop_ins {
        // Whatever the replaced unit file sets is replaced with it.
        return allowlisted_variables(manager, allowlist);
    }
    let loaded = unit.load_state == "loaded";
    let facts = scan_drop_ins(unit, unit_name, reader).await?;
    // An `EnvironmentFile=` only the replaced unit file holds does not count, so
    // the files are those the drop-ins give.
    let environment_files = read_drop_ins(unit, unit_name, reader)
        .await?
        .environment_files;
    let mut file_keys: Vec<(String, String)> = Vec::new();
    for (file, optional) in &environment_files {
        for key in environment_file_keys(file, *optional, unit_name, reader).await? {
            file_keys.push((key, file.clone()));
        }
    }
    let judgement = Judgement {
        unit,
        unit_name,
        allowlist,
        facts,
        file_keys,
        loaded,
    };
    judgement.refuse_bootstrap_overrides()?;
    judgement.refuse_unreadable_store_settings()?;
    judgement.apply(&mut manager)?;
    allowlisted_variables(manager, allowlist)
}
