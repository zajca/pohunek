//! Per-launch input a runtime's integration handler contributes to one launch.
//!
//! A [`LaunchContributor`] turns the facts of a launch (session, create/resume/
//! fork, resolved config home, effective environment, frozen arguments) into
//! pure data: environment pairs, root-position arguments and private files.
//! The contributor never touches the filesystem or the command. Core validates
//! the data, writes the files into the session's daemon-owned state directory
//! and resolves `{file:<name>}` placeholders in the environment values and
//! arguments to the absolute path of that file.
//!
//! The contribution is rebuilt for every launch and never persisted: it is not
//! part of the frozen resume snapshot, so a relaunch applies it exactly once.
//! The files live in `worker_state_root/<session>` as `launch-input.<name>`,
//! outside every directory the launched agent or its sandbox can write, and
//! are deleted with the session's journals.

// Rust guideline compliant 2026-10-27

use std::fmt::Debug;
use std::path::{Path, PathBuf};

use pohunek_platform::filesystem::TrustedDir;
use protocol::{ErrorClass, ProtocolError, SessionId};

/// Error code of a contribution core refuses to apply.
pub(crate) const LAUNCH_INPUT_INVALID_CODE: &str = "launch_input_invalid";

/// Error code of a contribution core cannot place.
pub(crate) const LAUNCH_INPUT_UNAVAILABLE_CODE: &str = "launch_input_unavailable";

/// Most private files one contribution may carry.
const MAX_FILES: usize = 4;

/// Largest private file, in bytes. Bounds what a contributor can make the
/// daemon write on every launch; a reporter script is the largest known file.
const MAX_FILE_BYTES: usize = 128 * 1024;

/// Most environment pairs one contribution may carry.
const MAX_ENV: usize = 8;

/// Most root-position arguments one contribution may carry.
const MAX_ARGS: usize = 8;

/// Longest environment value or argument, after placeholder resolution.
const MAX_VALUE_BYTES: usize = 4096;

/// Variable-name prefix the daemon reserves for its own handshake.
const RESERVED_ENV_PREFIX: &str = "POHUNEK_";

/// Opening of a path placeholder in an environment value or argument.
const PLACEHOLDER_OPEN: &str = "{file:";

/// Closing of a path placeholder.
const PLACEHOLDER_CLOSE: char = '}';

/// Mode of a data file: the owner reads and writes.
const DATA_MODE: u32 = 0o600;

/// Mode of an executable file: the owner reads and executes.
const EXECUTABLE_MODE: u32 = 0o500;

/// Mode of the worker state root and of every session directory in it.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Which launch the contribution is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchKind {
    /// A new session.
    Create,
    /// A relaunch of a stopped session.
    Resume,
    /// A new session forked from an existing one.
    Fork,
}

/// What a contributor may read about the launch it contributes to.
#[derive(Debug)]
pub struct LaunchInputContext<'a> {
    /// The session being launched.
    pub session_id: &'a SessionId,
    /// Create, resume or fork.
    pub kind: LaunchKind,
    /// The config home the runtime's descriptor declares, resolved against the
    /// environment the launched agent sees; `None` when the descriptor
    /// declares none or the environment names no usable home.
    pub config_home: Option<&'a Path>,
    /// The effective launch environment before the contribution: descriptor
    /// pairs, the private `TMPDIR`, the profile `[env]` and the daemon
    /// handshake. Secret-bearing; a contributor must not copy it elsewhere.
    pub env: &'a [(String, String)],
    /// The frozen launch arguments (definition or profile arguments).
    pub args: &'a [String],
}

/// How a private file is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// Owner read and write.
    Data,
    /// Owner read and execute.
    Executable,
}

/// A file core writes into the session's private launch-input directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateFile {
    /// Name below the launch-input prefix; see
    /// [`pohunek_paths::valid_launch_input_name`].
    pub name: String,
    /// File content.
    pub bytes: Vec<u8>,
    /// Protection of the file.
    pub kind: FileKind,
}

/// What a contributor adds to a launch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchInput {
    /// Environment pairs applied after the profile `[env]`. Values may name a
    /// private file as `{file:<name>}`. Names may not start with `POHUNEK_`.
    pub env: Vec<(String, String)>,
    /// Arguments placed after the frozen arguments and before the native
    /// session arguments. May name a private file as `{file:<name>}`.
    pub root_args: Vec<String>,
    /// Private files core writes before the launch.
    pub files: Vec<PrivateFile>,
}

/// A contribution after placement: placeholders resolved, files written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlacedLaunchInput {
    /// Environment pairs to apply after the profile `[env]`.
    pub env: Vec<(String, String)>,
    /// Arguments to place before the native session arguments.
    pub root_args: Vec<String>,
}

/// Computes the input one runtime adds to a launch.
pub trait LaunchContributor: Send + Sync + Debug {
    /// The input for the launch described by `ctx`.
    ///
    /// # Errors
    ///
    /// A typed error refuses the launch before any worker starts.
    fn contribute(&self, ctx: &LaunchInputContext<'_>) -> Result<LaunchInput, ProtocolError>;
}

/// `launch_input_invalid` naming what is wrong with the contribution.
fn invalid(detail: impl std::fmt::Display) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        LAUNCH_INPUT_INVALID_CODE,
        format!("the runtime's launch input is invalid: {detail}"),
        None,
    )
}

/// `launch_input_unavailable` for a failure to write a private file.
fn unavailable(detail: impl std::fmt::Display) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        LAUNCH_INPUT_UNAVAILABLE_CODE,
        format!("the runtime's launch input cannot be placed: {detail}"),
        Some("check that the Pohunek state directory is writable and not full".to_owned()),
    )
}

/// Checks the shape of `input` without touching the filesystem.
fn validate(input: &LaunchInput) -> Result<(), ProtocolError> {
    if input.files.len() > MAX_FILES {
        return Err(invalid(format!("more than {MAX_FILES} files")));
    }
    if input.env.len() > MAX_ENV {
        return Err(invalid(format!("more than {MAX_ENV} environment pairs")));
    }
    if input.root_args.len() > MAX_ARGS {
        return Err(invalid(format!("more than {MAX_ARGS} arguments")));
    }
    for (index, file) in input.files.iter().enumerate() {
        if !pohunek_paths::valid_launch_input_name(&file.name) {
            return Err(invalid(format!("file name {:?} is not allowed", file.name)));
        }
        if file.bytes.len() > MAX_FILE_BYTES {
            return Err(invalid(format!(
                "file {:?} is larger than {MAX_FILE_BYTES} bytes",
                file.name
            )));
        }
        if input.files[..index]
            .iter()
            .any(|other| other.name == file.name)
        {
            return Err(invalid(format!("file {:?} is listed twice", file.name)));
        }
    }
    for (name, _value) in &input.env {
        let acceptable = !name.is_empty()
            && !name.starts_with(RESERVED_ENV_PREFIX)
            && name
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            && !name.starts_with(|first: char| first.is_ascii_digit());
        if !acceptable {
            return Err(invalid(format!(
                "environment name {name:?} is not an upper-case name outside {RESERVED_ENV_PREFIX}*"
            )));
        }
    }
    Ok(())
}

/// Replaces every `{file:<name>}` in `text` with the absolute path of that
/// file below `directory`.
///
/// An unknown file name, an unterminated placeholder, a path that is not
/// UTF-8, an embedded NUL or a result longer than [`MAX_VALUE_BYTES`] refuses
/// the contribution.
fn resolve_placeholders(
    text: &str,
    directory: &Path,
    files: &[PrivateFile],
) -> Result<String, ProtocolError> {
    let mut resolved = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(PLACEHOLDER_OPEN) {
        resolved.push_str(&rest[..start]);
        let after = &rest[start + PLACEHOLDER_OPEN.len()..];
        let Some(end) = after.find(PLACEHOLDER_CLOSE) else {
            return Err(invalid("a file placeholder is not closed"));
        };
        let name = &after[..end];
        if !files.iter().any(|file| file.name == name) {
            return Err(invalid(format!("placeholder names unknown file {name:?}")));
        }
        let path = directory.join(pohunek_paths::launch_input_file_name(name));
        let path = path
            .to_str()
            .ok_or_else(|| invalid("a private file path is not UTF-8"))?;
        resolved.push_str(path);
        rest = &after[end + PLACEHOLDER_CLOSE.len_utf8()..];
    }
    resolved.push_str(rest);
    if resolved.contains('\0') {
        return Err(invalid("a value holds a NUL byte"));
    }
    if resolved.len() > MAX_VALUE_BYTES {
        return Err(invalid(format!(
            "a value is longer than {MAX_VALUE_BYTES} bytes"
        )));
    }
    Ok(resolved)
}

/// The session's launch-input directory below `state_root`, created owner-only.
fn open_session_directory(
    state_root: &Path,
    session_id: &SessionId,
) -> Result<(TrustedDir, PathBuf), ProtocolError> {
    if pohunek_paths::valid_worker_session_id(&session_id.0).is_none() {
        return Err(invalid(
            "the session id is not a valid state directory name",
        ));
    }
    let root =
        TrustedDir::open_or_create_absolute(state_root, PRIVATE_DIR_MODE).map_err(unavailable)?;
    let directory = root
        .open_or_create_child(&session_id.0, PRIVATE_DIR_MODE)
        .map_err(unavailable)?;
    Ok((directory, state_root.join(&session_id.0)))
}

/// Validates `input`, writes its files below `state_root` and resolves its
/// placeholders.
///
/// Each file is written atomically over any earlier launch's copy, so a name
/// that an attacker pre-planted as a symlink is replaced, never followed.
///
/// # Errors
///
/// `launch_input_invalid` for a contribution outside the bounds above, and
/// `launch_input_unavailable` when a file cannot be written.
pub(crate) fn place(
    input: LaunchInput,
    state_root: &Path,
    session_id: &SessionId,
) -> Result<PlacedLaunchInput, ProtocolError> {
    validate(&input)?;
    let (directory, path) = open_session_directory(state_root, session_id)?;
    let env = input
        .env
        .iter()
        .map(|(name, value)| {
            resolve_placeholders(value, &path, &input.files).map(|value| (name.clone(), value))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let root_args = input
        .root_args
        .iter()
        .map(|arg| resolve_placeholders(arg, &path, &input.files))
        .collect::<Result<Vec<_>, _>>()?;
    for file in &input.files {
        let mode = match file.kind {
            FileKind::Data => DATA_MODE,
            FileKind::Executable => EXECUTABLE_MODE,
        };
        directory
            .replace_file(
                pohunek_paths::launch_input_file_name(&file.name),
                pohunek_paths::launch_input_temp_name(&file.name),
                &file.bytes,
                mode,
            )
            .map_err(unavailable)?;
    }
    Ok(PlacedLaunchInput { env, root_args })
}
