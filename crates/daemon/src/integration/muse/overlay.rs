//! The per-launch Muse managed hooks overlay and the inputs it merges.
//!
//! The overlay keeps every hook of a foreign managed hooks file verbatim and
//! appends the Pohunek reporter per event. A foreign file that is present but
//! cannot be read, is too large or does not parse is an error: dropping it
//! would drop policy hooks.

// Rust guideline compliant 2026-10-27

use std::fs::{File, OpenOptions};
use std::io::Read as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use protocol::{ErrorClass, ProtocolError};
use serde_json::{json, Map, Value};
use thiserror::Error;

use super::ARG_CONFLICT_CODE;

/// Environment variable that replaces `managed_hooks_path` for one Muse run.
pub(super) const MANAGED_PATH_ENV: &str = "TBH_MANAGED_HOOKS_PATH";

/// Settings key naming the managed hooks file.
const MANAGED_PATH_KEY: &str = "managed_hooks_path";

/// Settings key and `-c` key listing the variables managed hooks receive.
const ENV_VARS_KEY: &str = "managed_hooks_env_vars";

/// Settings file below the Muse config home.
const SETTINGS_FILE: &str = "settings.json";

/// Largest foreign managed hooks file Pohunek merges, in bytes. A hooks file
/// is a few lines per event; the bound keeps a launch from reading an
/// arbitrary file.
const MAX_FOREIGN_BYTES: u64 = 64 * 1024;

/// Largest settings file Pohunek reads, in bytes.
const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;

/// Name the overlay is reported under when no foreign file is merged.
const OVERLAY_NAME: &str = "the Pohunek overlay";

/// Seconds Muse waits for the reporter before it gives up on the hook.
const HOOK_TIMEOUT_SECS: u64 = 10;

/// `(Muse event, reporter action)` pairs the overlay registers.
const EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session"),
    ("SubagentStart", "subagent-start"),
    ("SubagentStop", "subagent-stop"),
];

/// The `POHUNEK_*` names the reporter reads from the hook environment.
///
/// Muse clears the hook environment, so these are the only Pohunek variables
/// the reporter sees. A scenario compares this list with the names the
/// reporter asset reads.
pub(super) const HOOK_ENV_VARS: &[&str] = &[
    "POHUNEK_ENV",
    "POHUNEK_SESSION_ID",
    "POHUNEK_WORKER_SOCKET_PATH",
    "POHUNEK_WORKER_INSTANCE_ID",
    "POHUNEK_HOOK_TRACE_PATH",
    "POHUNEK_HOOK_TRACE_MAX_BYTES",
    "POHUNEK_NATIVE_REFERENCE_KIND",
];

/// Names beyond the reporter's own that the hook environment receives: `PATH`,
/// so the reporter finds the `python3` the launch environment resolves.
const INHERITED_VARS: &[&str] = &["PATH"];

/// Why the overlay inputs cannot be used.
#[derive(Debug, Error)]
pub(super) enum Problem {
    /// A file cannot be opened or read.
    #[error("{what} {path} cannot be read: {source}")]
    Unreadable {
        what: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    /// A file is not a regular file.
    #[error("{what} {path} is not a regular file")]
    NotRegular { what: &'static str, path: PathBuf },
    /// A file is larger than its bound.
    #[error("{what} {path} is larger than {limit} bytes")]
    TooLarge {
        what: &'static str,
        path: PathBuf,
        limit: u64,
    },
    /// A file is not the JSON object Muse expects.
    #[error("{what} {path} is not a JSON object")]
    NotObject { what: &'static str, path: PathBuf },
    /// A value has the wrong shape.
    #[error("{what} {path} holds an invalid {key}")]
    InvalidValue {
        what: &'static str,
        path: PathBuf,
        key: &'static str,
    },
    /// The overlay cannot be encoded.
    #[error("the Muse hook overlay cannot be encoded: {0}")]
    Encode(#[from] serde_json::Error),
    /// The managed hooks path is not absolute.
    #[error("the managed hooks path {0:?} is not an absolute path")]
    RelativePath(String),
}

/// What the overlay merges: a foreign managed file and the user's own
/// variable list.
#[derive(Debug, Default)]
pub(super) struct Inputs {
    /// The parsed foreign managed hooks file and its path, when one is selected.
    foreign: Option<(PathBuf, Map<String, Value>)>,
    /// The `managed_hooks_env_vars` of the user's settings.
    user_vars: Vec<String>,
}

impl Inputs {
    /// Reads the settings of `config_home` and the foreign managed file the
    /// launch environment `env`, else the settings, selects.
    pub(super) fn read(
        config_home: Option<&Path>,
        env: &[(String, String)],
    ) -> Result<Self, Problem> {
        let mut inputs = Self::default();
        // The last entry wins; an empty value means the variable is unset.
        let mut selected = env
            .iter()
            .rev()
            .find(|(name, _value)| name == MANAGED_PATH_ENV)
            .map(|(_name, value)| value.clone())
            .filter(|value| !value.is_empty());
        if let Some(home) = config_home {
            let path = home.join(SETTINGS_FILE);
            if let Some(settings) =
                read_object(&path, "Muse settings file", MAX_SETTINGS_BYTES, true)?
            {
                if selected.is_none() {
                    selected = match settings.get(MANAGED_PATH_KEY) {
                        None | Some(Value::Null) => None,
                        Some(Value::String(path)) => Some(path.clone()),
                        Some(_) => {
                            return Err(Problem::InvalidValue {
                                what: "Muse settings file",
                                path,
                                key: MANAGED_PATH_KEY,
                            })
                        }
                    };
                }
                inputs.user_vars = user_vars(&settings, &path)?;
            }
        }
        if let Some(selected) = selected.filter(|selected| !selected.is_empty()) {
            let foreign = PathBuf::from(&selected);
            if !foreign.is_absolute() {
                return Err(Problem::RelativePath(selected));
            }
            let object = read_object(
                &foreign,
                "Muse managed hooks file",
                MAX_FOREIGN_BYTES,
                false,
            )?
            .unwrap_or_default();
            inputs.foreign = Some((foreign, object));
        }
        Ok(inputs)
    }
}

/// The user's `managed_hooks_env_vars` list.
fn user_vars(settings: &Map<String, Value>, path: &Path) -> Result<Vec<String>, Problem> {
    let invalid = || Problem::InvalidValue {
        what: "Muse settings file",
        path: path.to_path_buf(),
        key: ENV_VARS_KEY,
    };
    match settings.get(ENV_VARS_KEY) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::String(name) if is_variable_name(name) => Ok(name.clone()),
                _ => Err(invalid()),
            })
            .collect(),
        Some(_) => Err(invalid()),
    }
}

/// Whether `name` is a plain environment variable name.
fn is_variable_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|first: char| first.is_ascii_digit())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Reads `path` as a JSON object; `None` when it does not exist and `optional`.
///
/// The path may be a symlink, as a dotfile manager makes it; the opened file
/// must be a regular file. `O_NONBLOCK` keeps a FIFO from blocking the launch.
fn read_object(
    path: &Path,
    what: &'static str,
    limit: u64,
    optional: bool,
) -> Result<Option<Map<String, Value>>, Problem> {
    let unreadable = |source| Problem::Unreadable {
        what,
        path: path.to_path_buf(),
        source,
    };
    let file: File = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(unreadable(error)),
    };
    let metadata = file.metadata().map_err(unreadable)?;
    if !metadata.is_file() {
        return Err(Problem::NotRegular {
            what,
            path: path.to_path_buf(),
        });
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    if bytes.len() as u64 > limit {
        return Err(Problem::TooLarge {
            what,
            path: path.to_path_buf(),
            limit,
        });
    }
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(object)) => Ok(Some(object)),
        _ => Err(Problem::NotObject {
            what,
            path: path.to_path_buf(),
        }),
    }
}

/// The `-c` value that lists the reporter's variables and the user's own.
pub(super) fn env_vars_arg(inputs: &Inputs) -> String {
    let mut names: Vec<&str> = HOOK_ENV_VARS
        .iter()
        .chain(INHERITED_VARS)
        .copied()
        .collect();
    for name in &inputs.user_vars {
        if !names.contains(&name.as_str()) {
            names.push(name);
        }
    }
    format!("{ENV_VARS_KEY}={}", json!(names))
}

/// Refuses a launch whose arguments already set the variable list, which Muse
/// refuses as a duplicate `-c` key. Every spelling that contains the key is
/// refused, because the accepted spellings are not documented.
pub(super) fn refuse_conflicting_args(args: &[String]) -> Result<(), ProtocolError> {
    if args.iter().any(|arg| arg.contains(ENV_VARS_KEY)) {
        return Err(ProtocolError::new(
            ErrorClass::Runtime,
            ARG_CONFLICT_CODE,
            format!("the launch arguments already set {ENV_VARS_KEY}, which the Muse hook overlay needs to set itself"),
            Some(format!(
                "remove {ENV_VARS_KEY} from the runtime or profile arguments; list extra variables in the Muse settings file instead"
            )),
        ));
    }
    Ok(())
}

/// The command line that runs the reporter for `action`: the absolute path in
/// single quotes, then the action.
fn command(reporter: &Path, action: &str) -> String {
    let quoted = reporter.to_string_lossy().replace('\'', "'\\''");
    format!("'{quoted}' {action}")
}

/// The overlay file: the foreign hooks verbatim plus the reporter per event.
pub(super) fn build(inputs: &Inputs, reporter: &Path) -> Result<Vec<u8>, Problem> {
    let (foreign_path, mut root) = inputs
        .foreign
        .clone()
        .unwrap_or_else(|| (PathBuf::from(OVERLAY_NAME), Map::new()));
    let invalid = |key| Problem::InvalidValue {
        what: "Muse managed hooks file",
        path: foreign_path.clone(),
        key,
    };
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(hooks) = hooks else {
        return Err(invalid("hooks"));
    };
    for (event, action) in EVENTS {
        let entries = hooks
            .entry((*event).to_owned())
            .or_insert_with(|| Value::Array(Vec::new()));
        let Value::Array(entries) = entries else {
            return Err(invalid("hooks"));
        };
        entries.push(json!({
            "hooks": [{
                "type": "command",
                "command": command(reporter, action),
                "timeout": HOOK_TIMEOUT_SECS,
            }],
        }));
    }
    root.entry("schema_version").or_insert_with(|| json!(1));
    Ok(serde_json::to_vec_pretty(&Value::Object(root))?)
}
