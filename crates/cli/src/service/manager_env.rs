//! The environment the service manager gives the daemon job.
//!
//! The daemon's systemd user unit records no `PATH` when the install chose
//! unmanaged discovery, so the daemon inherits the user manager's `PATH`. The
//! same manager environment carries the allowlisted variables a daemon reads,
//! such as a store location, so a preflight judges with them too. A manager
//! that cannot answer is an error: the caller never falls back to an empty
//! `PATH` or to the CLI's own environment. Values are never part of an error.

// Rust guideline compliant 2026-10-14

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;

use pohunek_platform::supervisor::UnitEnvironment;
use pohunek_platform::supervisor::SEARCH_PATH_VARIABLE;
use pohunek_worker_protocol::BaseEnv;

use super::error::Error;

mod job_variables;

pub use job_variables::unit_job_variables;

/// Why the service manager's environment was not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentFailure {
    /// No service manager answers: its bus cannot be reached. The job then has
    /// no manager environment to inherit from, and the caller decides what that
    /// means for its own step.
    Unreachable(String),
    /// The manager answered, but its answer cannot be used.
    Unusable(String),
}

/// Reads the environment the systemd user manager gives its units.
pub trait UserManager: Debug + Send + Sync {
    /// Returns the `KEY=VALUE` entries of the manager's environment, or why they
    /// cannot be read.
    fn environment(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, EnvironmentFailure>> + Send + '_>>;

    /// Returns the environment settings the manager holds for `unit`, drop-ins
    /// included, or why they cannot be read.
    fn unit_environment<'a>(
        &'a self,
        unit: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<UnitEnvironment, EnvironmentFailure>> + Send + 'a>>;
}

/// Reads the environment through the platform's D-Bus client on Linux.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemdUserManager;

impl UserManager for SystemdUserManager {
    #[cfg(target_os = "linux")]
    fn environment(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, EnvironmentFailure>> + Send + '_>> {
        use super::settings;

        Box::pin(async {
            pohunek_platform::supervisor::systemd::user_manager_environment(
                settings::MANAGER_ENVIRONMENT_TIMEOUT,
            )
            .await
            .map_err(|error| bus_failure(&error))
        })
    }

    #[cfg(target_os = "linux")]
    fn unit_environment<'a>(
        &'a self,
        unit: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<UnitEnvironment, EnvironmentFailure>> + Send + 'a>>
    {
        use super::settings;

        Box::pin(async move {
            pohunek_platform::supervisor::systemd::unit_environment(
                unit,
                settings::MANAGER_ENVIRONMENT_TIMEOUT,
            )
            .await
            .map_err(|error| bus_failure(&error))
        })
    }

    /// The systemd user manager exists only on Linux.
    #[cfg(not(target_os = "linux"))]
    fn environment(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, EnvironmentFailure>> + Send + '_>> {
        Box::pin(std::future::ready(Err(EnvironmentFailure::Unreachable(
            "the systemd user manager exists only on Linux".to_owned(),
        ))))
    }

    /// The systemd user manager exists only on Linux.
    #[cfg(not(target_os = "linux"))]
    fn unit_environment<'a>(
        &'a self,
        _unit: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<UnitEnvironment, EnvironmentFailure>> + Send + 'a>>
    {
        Box::pin(std::future::ready(Err(EnvironmentFailure::Unreachable(
            "the systemd user manager exists only on Linux".to_owned(),
        ))))
    }
}

/// Classifies a failed bus read.
///
/// Only an absent bus means no manager is reachable; a bus that refuses the
/// client is an answer that cannot be used.
#[cfg(target_os = "linux")]
fn bus_failure(error: &pohunek_platform::supervisor::Error) -> EnvironmentFailure {
    let detail = if error.is_bus_address_invalid() {
        "the session bus address cannot be used (zbus address error)".to_owned()
    } else {
        error.to_string()
    };
    if error.is_no_bus() {
        EnvironmentFailure::Unreachable(detail)
    } else {
        EnvironmentFailure::Unusable(detail)
    }
}

/// What the systemd user manager gives the daemon job, read once.
#[derive(Clone, PartialEq, Eq)]
pub struct UserManagerEnvironment {
    /// The manager's `PATH`, byte for byte, when it sets one. The daemon job
    /// runs with exactly this value.
    pub search_path: Option<String>,
    /// The manager's variables the allowlist selects, `PATH` excluded.
    pub variables: BTreeMap<String, String>,
}

// Variables are printed by name only: their values may be secrets.
impl Debug for UserManagerEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserManagerEnvironment")
            .field("search_path", &self.search_path)
            .field("variables", &self.variables.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// Reads the environment of the systemd user manager through `manager` once.
///
/// # Errors
///
/// Returns the manager's [`EnvironmentFailure`].
pub async fn read_user_manager_entries(
    manager: &dyn UserManager,
) -> Result<Vec<String>, EnvironmentFailure> {
    manager.environment().await
}

/// Reads the environment of the systemd user manager through `manager` once,
/// and selects its variables with `allowlist`. Every failure to read it is an
/// error: a caller that may go on without the manager uses
/// [`read_user_manager_entries`] instead.
///
/// # Errors
///
/// Returns [`Error::ManagerEnvironment`] when the environment cannot be read or
/// an allowlisted variable cannot be carried by a job environment.
pub async fn user_manager_environment(
    manager: &dyn UserManager,
    allowlist: &[String],
) -> Result<UserManagerEnvironment, Error> {
    let entries = read_user_manager_entries(manager)
        .await
        .map_err(|failure| {
            let (EnvironmentFailure::Unreachable(detail) | EnvironmentFailure::Unusable(detail)) =
                failure;
            Error::ManagerEnvironment { detail }
        })?;
    selected_environment(&entries, allowlist)
}

/// Selects the `PATH` and the allowlisted variables of the manager's `entries`.
///
/// # Errors
///
/// Returns [`Error::ManagerEnvironment`] when an allowlisted variable cannot be
/// carried by a job environment.
pub fn selected_environment(
    entries: &[String],
    allowlist: &[String],
) -> Result<UserManagerEnvironment, Error> {
    let prefix = format!("{SEARCH_PATH_VARIABLE}=");
    let search_path = entries
        .iter()
        .find_map(|entry| entry.strip_prefix(prefix.as_str()))
        .map(carry)
        .transpose()
        .map_err(|detail| Error::ManagerEnvironment { detail })?;
    let pairs = entries.iter().filter_map(|entry| {
        entry
            .split_once('=')
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
    });
    Ok(UserManagerEnvironment {
        search_path,
        variables: allowlisted_variables(pairs, allowlist)?,
    })
}

/// Selects the variables of a service manager environment that `allowlist`
/// names, with the job's own `PATH` left out.
///
/// The selection is the one a daemon applies to its launch environment, so a
/// variable the daemon would not inherit is never passed to a preflight. Errors
/// name variables, never their values.
///
/// # Errors
///
/// Returns [`Error::ManagerEnvironment`] for an invalid pattern or a selected
/// variable that a base environment cannot hold.
pub fn allowlisted_variables(
    entries: impl IntoIterator<Item = (String, String)>,
    allowlist: &[String],
) -> Result<BTreeMap<String, String>, Error> {
    let vars = entries
        .into_iter()
        .filter(|(name, _)| name != SEARCH_PATH_VARIABLE)
        .map(|(name, value)| (OsString::from(name), OsString::from(value)));
    let base =
        BaseEnv::from_allowlist(allowlist, vars).map_err(|error| Error::ManagerEnvironment {
            detail: format!("the service manager's environment cannot be carried: {error}"),
        })?;
    Ok(base.into_inner())
}

/// The unit setting files read while one judgement resolves a daemon job's
/// environment, each at most once, so every part of the judgement sees the same
/// bytes.
#[derive(Default)]
pub struct UnitFileReader {
    unit_files: BTreeMap<String, Vec<u8>>,
    drop_ins: BTreeMap<String, Option<Vec<u8>>>,
    environment_files: BTreeMap<String, Result<Vec<u8>, (std::io::ErrorKind, String)>>,
}

// The files may hold secrets: only their paths are printed.
impl Debug for UnitFileReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnitFileReader")
            .field("unit_files", &self.unit_files.keys().collect::<Vec<_>>())
            .field("drop_ins", &self.drop_ins.keys().collect::<Vec<_>>())
            .field(
                "environment_files",
                &self.environment_files.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// The `PATH` value of a job, if a process environment can carry it.
///
/// A value with a NUL byte cannot be placed in a process environment. Every
/// other byte sequence is passed as it is.
///
/// # Errors
///
/// Returns a message for a value with an interior NUL byte.
pub(crate) fn carry(value: &str) -> Result<String, String> {
    if value.contains('\0') {
        return Err(
            "its PATH contains a NUL byte, which a process environment cannot carry".to_owned(),
        );
    }
    Ok(value.to_owned())
}

/// Returns `text` unchanged, or the first specifier in it as the `Err`: the
/// manager expands a loaded unit's specifiers itself, but for a unit it has not
/// loaded the values it would use (`%h`, `%t`, ...) cannot be read here, so none
/// is expanded.
fn without_specifiers(text: &str) -> Result<String, String> {
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            return Err(format!(
                "%{}",
                chars.next().map(String::from).unwrap_or_default()
            ));
        }
    }
    Ok(text.to_owned())
}

/// The `PATH` the daemon job gets from the user manager and its own unit.
///
/// `manager_path` is the manager's `PATH`; `unit` is what the manager holds for
/// the daemon unit (`unit_name`), drop-ins merged. The job's `PATH` is, in
/// systemd's order, the manager's, replaced by `ExecSearchPath=` (joined with
/// `:`), replaced by an `Environment=` entry for `PATH`; the last entry wins.
/// The install replaces the unit file itself and keeps its drop-ins, so a
/// setting counts only when a drop-in gives it: the merged property is attributed
/// by reading the unit file, and when the unit file sets the same key the
/// drop-ins are read to find one that does too. Every setting that could change
/// `PATH` but whose result is not derivable here is refused, with the cause and
/// the directive to change, so the caller never judges a `PATH` the job does not
/// get. Each file is read once, bounded; values are never part of an error.
///
/// # Errors
///
/// Returns [`Error::ManagerEnvironment`] for a stale snapshot next to drop-ins,
/// a unit that is masked or failed to load, drop-ins of a unit with no file, an
/// `EnvironmentFile=` that sets `PATH` or cannot be followed, a unit file or
/// drop-in that cannot be read or attributed, and `PassEnvironment=` or
/// `UnsetEnvironment=` naming `PATH`.
pub async fn effective_search_path(
    manager_path: Option<String>,
    unit: &UnitEnvironment,
    unit_name: &str,
    reader: &mut UnitFileReader,
) -> Result<Option<String>, Error> {
    if unit.load_state == "not-found"
        && (!unit.drop_in_paths.is_empty() || !unit.drop_ins_on_disk.is_empty())
    {
        return path_from_orphan_drop_ins(manager_path, unit, unit_name, reader).await;
    }
    if !unit_can_set_path(unit, unit_name, reader, true).await? {
        return Ok(manager_path);
    }
    let loaded = loaded_search_path(manager_path.clone(), unit, unit_name, reader).await?;
    if !unit.drop_in_paths.is_empty() || !unit.drop_ins_on_disk.is_empty() {
        // A rollback or a config migration removes the unit and resolves again from
        // its drop-in files alone. Resolve that way now, before anything is
        // stopped, and refuse what it cannot read or resolves differently (for
        // instance a specifier the manager and the CLI expand differently).
        let removed = UnitEnvironment {
            load_state: "not-found".to_owned(),
            drop_in_paths: unit.drop_in_paths.clone(),
            drop_ins_on_disk: unit.drop_ins_on_disk.clone(),
            ..UnitEnvironment::default()
        };
        let after = path_from_orphan_drop_ins(manager_path, &removed, unit_name, reader).await?;
        if after != loaded {
            return Err(Error::ManagerEnvironment {
                detail: format!(
                    "the {SEARCH_PATH_VARIABLE} that {unit_name} gets from its drop-ins differs from the one resolved from the same files once the unit is removed (a rollback or config migration removes it), so a recovery would judge another {SEARCH_PATH_VARIABLE}. Write the drop-in settings without specifiers or file overrides that differ between the two (`systemctl --user cat {unit_name}`) and retry"
                ),
            });
        }
    }
    Ok(loaded)
}

/// The environment the daemon job gets from the unit and the user manager.
#[derive(Clone, PartialEq, Eq)]
pub struct JobEnvironment {
    /// The job's `PATH`.
    pub search_path: String,
    /// The drop-in that replaces a recorded `PATH`, when one does.
    pub search_path_drop_in: Option<String>,
    /// The allowlisted variables, `PATH` excluded.
    pub variables: BTreeMap<String, String>,
}

// Variables are printed by name only: their values may be secrets.
impl Debug for JobEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobEnvironment")
            .field("search_path", &self.search_path)
            .field("search_path_drop_in", &self.search_path_drop_in)
            .field("variables", &self.variables.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// The environment the daemon job `unit_name` gets: its `PATH` and its
/// allowlisted variables, from the user manager's `entries` and the unit the
/// manager reports. `recorded` is the `PATH` `service.toml` records, which the
/// unit file the install writes carries; without one the job inherits the
/// manager's. Each unit setting file is read once.
///
/// # Errors
///
/// Returns [`Error::ManagerEnvironment`] for everything
/// [`effective_search_path`], [`recorded_search_path`] and
/// [`unit_job_variables`] refuse.
pub async fn effective_job_environment(
    entries: &[String],
    unit: &UnitEnvironment,
    unit_name: &str,
    allowlist: &[String],
    recorded: Option<&str>,
) -> Result<JobEnvironment, Error> {
    let mut reader = UnitFileReader::default();
    let manager = selected_environment(entries, allowlist)?;
    let (search_path, search_path_drop_in) = if let Some(recorded) = recorded {
        let path = recorded_search_path(recorded, unit, unit_name, &mut reader).await?;
        (path.value, path.drop_in)
    } else {
        let path = effective_search_path(manager.search_path, unit, unit_name, &mut reader)
            .await?
            .ok_or_else(|| Error::ManagerEnvironment {
                detail: "it sets no PATH entry".to_owned(),
            })?;
        (path, None)
    };
    let variables = unit_job_variables(entries, unit, unit_name, allowlist, &mut reader).await?;
    Ok(JobEnvironment {
        search_path,
        search_path_drop_in,
        variables,
    })
}

/// The job's `PATH` from the manager's merged properties of a loaded unit.
async fn loaded_search_path(
    manager_path: Option<String>,
    unit: &UnitEnvironment,
    unit_name: &str,
    reader: &mut UnitFileReader,
) -> Result<Option<String>, Error> {
    let refuse = |detail: String| Error::ManagerEnvironment { detail };
    let prefix = format!("{SEARCH_PATH_VARIABLE}=");
    let merged_env = unit
        .environment
        .iter()
        .rev()
        .find_map(|entry| entry.strip_prefix(prefix.as_str()));
    let merged_exec = (!unit.exec_search_path.is_empty()).then(|| unit.exec_search_path.join(":"));
    if merged_env.is_none() && merged_exec.is_none() {
        return Ok(manager_path);
    }

    let unit_file = if unit.fragment_path.is_empty() {
        Vec::new()
    } else {
        reader.unit_file(&unit.fragment_path, unit_name).await?
    };
    let mut drop_ins = Vec::with_capacity(unit.drop_in_paths.len());
    for path in &unit.drop_in_paths {
        // The manager keeps a link to `/dev/null` in its list, and it disables
        // the drop-in of that name.
        if let Some(bytes) = reader.drop_in(path, unit_name).await? {
            drop_ins.push((path, bytes));
        }
    }
    let given_by_drop_in = |setting: Setting| -> Result<bool, Error> {
        if drop_ins.is_empty() {
            // Nothing the install keeps can give it: a merged value is the
            // replaced unit file's, possibly an older copy of it.
            return Ok(false);
        }
        if unit_setting(&unit_file, setting) == Assignment::No {
            // The unit file does not set it, so the merged value is a drop-in's.
            return Ok(true);
        }
        let mut unclear = None;
        for (path, text) in &drop_ins {
            match unit_setting(text, setting) {
                Assignment::Sets if matches!(setting, Setting::ExecSearchPath) => {
                    // The directive appends, so the merged list holds the unit
                    // file's entries too, which the install replaces.
                    return Err(refuse(format!(
                        "{unit_name} gets {} from both its unit file, which the install replaces, and the drop-in {path}, and the merged list cannot be split into the part that stays. Keep the entries in one drop-in and remove the others, and retry",
                        setting.directive()
                    )));
                }
                Assignment::Sets => return Ok(true),
                Assignment::Unclear => unclear = Some(path),
                Assignment::No => {}
            }
        }
        match unclear {
            Some(path) => Err(refuse(format!(
                "the drop-in {path} of {unit_name} has a {} line that may set it in a form not read here (an escape sequence, a specifier systemd drops, bytes that are not UTF-8, or a quote never closed), while {unit_name} also gets it from its unit file, which the install replaces. Write a plain assignment or remove the other entry, and retry",
                setting.directive()
            ))),
            None => Err(refuse(format!(
                "{unit_name} sets {} in its unit file, which the install replaces, and none of its {} drop-in file(s) could be recognized as keeping it, so the {SEARCH_PATH_VARIABLE} its job gets cannot be attributed. Write the setting as a plain line in one drop-in and retry",
                setting.directive(),
                drop_ins.len()
            ))),
        }
    };
    if let Some(merged) = merged_env {
        if given_by_drop_in(Setting::EnvironmentPath)? {
            return carry(merged).map(Some).map_err(refuse);
        }
    }
    if let Some(merged) = merged_exec {
        if given_by_drop_in(Setting::ExecSearchPath)? {
            return carry(&merged).map(Some).map_err(refuse);
        }
    }
    Ok(manager_path)
}

impl UnitFileReader {
    /// Reads a drop-in, or `None` for one linked to `/dev/null`, which disables
    /// the drop-in of that name.
    async fn drop_in(&mut self, path: &str, unit_name: &str) -> Result<Option<Vec<u8>>, Error> {
        if let Some(read) = self.drop_ins.get(path) {
            return Ok(read.clone());
        }
        let read = if std::fs::canonicalize(path)
            .is_ok_and(|target| target == std::path::Path::new("/dev/null"))
        {
            None
        } else {
            Some(read_unit_setting_file(path, unit_name, "drop-in").await?)
        };
        self.drop_ins.insert(path.to_owned(), read.clone());
        Ok(read)
    }

    /// Reads the unit file the manager loaded.
    async fn unit_file(&mut self, path: &str, unit_name: &str) -> Result<Vec<u8>, Error> {
        if let Some(bytes) = self.unit_files.get(path) {
            return Ok(bytes.clone());
        }
        let bytes = read_unit_setting_file(path, unit_name, "unit file").await?;
        self.unit_files.insert(path.to_owned(), bytes.clone());
        Ok(bytes)
    }

    /// Reads an `EnvironmentFile=`; the read failure is the inner error.
    async fn environment_file(
        &mut self,
        path: &str,
        unit_name: &str,
    ) -> Result<std::io::Result<Vec<u8>>, Error> {
        if let Some(read) = self.environment_files.get(path) {
            return Ok(read
                .clone()
                .map_err(|(kind, message)| std::io::Error::new(kind, message)));
        }
        let read = read_unit_settings_file(path, unit_name, "EnvironmentFile=").await?;
        self.environment_files.insert(
            path.to_owned(),
            read.as_ref()
                .map(Clone::clone)
                .map_err(|error| (error.kind(), error.to_string())),
        );
        Ok(read)
    }
}

/// The drop-ins that apply, in file-name order (the order systemd applies them
/// in). `files` come in lookup priority: of a name that several directories
/// hold, the first wins, and a link to `/dev/null` as the winner disables it.
/// A name held by differently named directories (`a.service.d` and `service.d`,
/// say) has no priority derived here and is refused.
fn drop_ins_by_priority(files: &[String], unit_name: &str) -> Result<Vec<String>, Error> {
    let name_of = |path: &str| path.rsplit('/').next().unwrap_or(path).to_owned();
    let directory_of = |path: &str| path.rsplit('/').nth(1).unwrap_or_default().to_owned();
    let mut winners: Vec<&String> = Vec::new();
    for path in files {
        match winners.iter().find(|kept| name_of(kept) == name_of(path)) {
            None => winners.push(path),
            Some(kept) if directory_of(kept) == directory_of(path) => {}
            Some(kept) => {
                return Err(Error::ManagerEnvironment {
                    detail: format!(
                        "{unit_name}: the drop-in {} exists in two directories ({kept} and {path}) of different names, so which one systemd applies is not derived here. Remove one of them and retry",
                        name_of(path),
                    ),
                });
            }
        }
    }
    winners.sort_by_key(|path| name_of(path));
    Ok(winners.into_iter().cloned().collect())
}

/// The drop-in files of `unit` in lookup priority: those on disk, then any the
/// manager loaded that the scan did not list.
fn drop_in_files_of(unit: &UnitEnvironment) -> Vec<String> {
    let mut files = unit.drop_ins_on_disk.clone();
    for path in &unit.drop_in_paths {
        if !files.contains(path) {
            files.push(path.clone());
        }
    }
    files
}

/// Whether `path` is absolute and normalized: no empty, `.` or `..` component and
/// no trailing slash, the form systemd accepts for an `EnvironmentFile=`.
fn is_normalized_absolute(path: &str) -> bool {
    path.strip_prefix('/').is_some_and(|rest| {
        rest.is_empty()
            || rest
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..")
    })
}

/// Whether an `ExecSearchPath=` entry is one systemd applies as written: a
/// normalized absolute path of fewer than 4096 bytes with no component over 255.
fn is_usable_search_entry(entry: &str) -> bool {
    is_normalized_absolute(entry)
        && entry.len() < 4096
        && entry.split('/').all(|part| part.len() <= 255)
}

/// Whether a `PassEnvironment=` or `UnsetEnvironment=` value names `PATH`.
fn names_search_path(value: &str) -> bool {
    names_variable(value, SEARCH_PATH_VARIABLE)
}

/// Whether a `PassEnvironment=` or `UnsetEnvironment=` value names `name`: a
/// word that is exactly `name` (or `name=...`) once unquoted. An escape sequence
/// can spell the name, and so can a specifier systemd expands, so both count as
/// naming it; an open quote only when the value mentions `name`.
fn names_variable(value: &str, name: &str) -> bool {
    let split = split_words(value);
    if split.escaped
        || value.replace("%%", "").contains('%')
        || (split.open_quote && value.contains(name))
    {
        return true;
    }
    split.words.iter().any(|word| {
        word == name
            || word
                .strip_prefix(name)
                .is_some_and(|rest| rest.starts_with('='))
    })
}

/// Refuses a drop-in whose syntax is not followed here: a line opening like a
/// section header without closing it (systemd stops reading the file there), a
/// byte order mark after the start of the file.
fn refuse_unfollowed_syntax(bytes: &[u8], path: &str, unit_name: &str) -> Result<(), Error> {
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let lines = physical_lines(body);
    let reason = if body.windows(3).any(|window| window == [0xEF, 0xBB, 0xBF]) {
        "a byte order mark after the start of the file, which is not followed here"
    } else if lines.iter().any(|line| {
        let line = String::from_utf8_lossy(line);
        let line = line.trim_matches(SYSTEMD_WHITESPACE);
        line.starts_with('[') && !line.ends_with(']')
    }) {
        "a line that opens a section header without closing it, after which systemd reads nothing further"
    } else {
        return Ok(());
    };
    Err(Error::ManagerEnvironment {
        detail: format!(
            "{unit_name}: the drop-in {path} has {reason}. Write plain settings under a well-formed [Service] header and retry"
        ),
    })
}

/// Refuses an absolute `EnvironmentFile=` path that is not normalized: what
/// systemd reads for it is not followed here.
fn refuse_unnormalized_path(file: &str, path: &str, unit_name: &str) -> Result<(), Error> {
    if is_normalized_absolute(file) {
        return Ok(());
    }
    Err(Error::ManagerEnvironment {
        detail: format!(
            "{unit_name}: the drop-in {path} names the EnvironmentFile= {file}, which is not a normalized path (an empty, `.` or `..` component, or a trailing slash), so what systemd reads for it is not followed here. Write the normalized path and retry"
        ),
    })
}

/// The refusal for a `PassEnvironment=` or `UnsetEnvironment=` naming `PATH`.
fn refuse_path_filter(key: &str, path: &str, unit_name: &str) -> Error {
    Error::ManagerEnvironment {
        detail: format!(
            "{unit_name}: the drop-in {path} sets {key}= for {SEARCH_PATH_VARIABLE}, which the service manager cannot report once the unit is removed. Remove that line and retry"
        ),
    }
}

/// What the drop-in files of a unit say about the job's environment, read in
/// the order systemd applies them (file-name order across lookup directories).
#[derive(Debug, Default)]
struct DropInReading {
    /// The `EnvironmentFile=` paths in force, with their optional (`-`) flag.
    environment_files: Vec<(String, bool)>,
    /// The `ExecSearchPath=` entries in force.
    search_path: Vec<String>,
    /// The last `Environment=` event for `PATH`: the drop-in and its value, or
    /// `None` as the value for a reset.
    path_event: Option<(String, Option<String>)>,
    /// The `PassEnvironment=` and `UnsetEnvironment=` lines naming `PATH`, as the
    /// directive and the drop-in.
    path_filters: Vec<(String, String)>,
}

/// Reads the drop-in files of `unit` for the settings that can change the job's
/// `PATH`: they are the files an install keeps and applies. Refuses what it
/// cannot read as plain assignments, and every directive whose effect the
/// manager cannot report for an unloaded unit.
async fn read_drop_ins(
    unit: &UnitEnvironment,
    unit_name: &str,
    reader: &mut UnitFileReader,
) -> Result<DropInReading, Error> {
    let refuse = |detail: String| Error::ManagerEnvironment { detail };
    let files = drop_ins_by_priority(&drop_in_files_of(unit), unit_name)?;
    let mut reading = DropInReading::default();
    for path in &files {
        let Some(bytes) = reader.drop_in(path, unit_name).await? else {
            continue;
        };
        refuse_unfollowed_syntax(&bytes, path, unit_name)?;
        let unreadable = |directive: &str| {
            refuse(format!(
                "{unit_name}: the drop-in {path} has a {directive} line that may affect {SEARCH_PATH_VARIABLE} in a form not read here (an escape sequence, more than one word, a quote never closed, or bytes that are not UTF-8). Write a plain assignment and retry"
            ))
        };
        let unsupported = |directive: &str, specifier: &str| {
            refuse(format!(
                "{unit_name}: the drop-in {path} uses the specifier {specifier} in {directive}=, which is not expanded here: a recovery that removes the unit resolves its drop-ins without the service manager, so the value the manager would use cannot be read. Write the value out and retry"
            ))
        };
        for line in service_lines(&bytes) {
            let Ok(line) = line else {
                return Err(unreadable("Environment="));
            };
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim_end_matches(SYSTEMD_WHITESPACE);
            match key {
                "EnvironmentFile" | "ExecSearchPath" => {
                    if value.trim_matches(SYSTEMD_WHITESPACE).is_empty() {
                        if key == "EnvironmentFile" {
                            reading.environment_files.clear();
                        } else {
                            reading.search_path.clear();
                        }
                        continue;
                    }
                    let split = split_words(value);
                    if split.open_quote
                        || split.escaped
                        || (key == "ExecSearchPath"
                            && (split.words.len() != 1 || value.contains(['"', '\''])))
                    {
                        return Err(unreadable(key));
                    }
                    if key == "EnvironmentFile" {
                        // The whole value is one path, spaces included.
                        let raw = value.trim_matches(SYSTEMD_WHITESPACE);
                        if raw.contains(['"', '\'']) {
                            return Err(unreadable(key));
                        }
                        let raw = without_specifiers(raw)
                            .map_err(|specifier| unsupported(key, &specifier))?;
                        let optional = raw.starts_with('-');
                        let file = raw.strip_prefix('-').unwrap_or(&raw).to_owned();
                        // systemd ignores a relative path (pinned on the real
                        // manager); how it reads a non-normalized absolute one
                        // is not reproduced here.
                        if file.starts_with('/') {
                            refuse_unnormalized_path(&file, path, unit_name)?;
                            reading.environment_files.push((file, optional));
                        }
                    } else {
                        let list = without_specifiers(&split.words[0])
                            .map_err(|specifier| unsupported(key, &specifier))?;
                        for entry in list.split(':') {
                            if !is_usable_search_entry(entry) {
                                return Err(refuse(format!(
                                    "{unit_name}: the drop-in {path} has an entry in ExecSearchPath= that is empty, not a normalized absolute path or too long, which systemd ignores or simplifies (dropping the entries after it), so the list is not followed here. Write normalized absolute entries of at most 4095 bytes, with no component over 255 bytes, and retry"
                                )));
                            }
                            reading.search_path.push(entry.to_owned());
                        }
                    }
                }
                "PassEnvironment" | "UnsetEnvironment" if names_search_path(value) => {
                    reading.path_filters.push((key.to_owned(), path.clone()));
                }
                _ => {}
            }
        }
        let events = match path_events(&bytes) {
            Ok(events) => events,
            Err(Some(specifier)) => return Err(unsupported("Environment", &specifier)),
            Err(None) => return Err(unreadable("Environment=")),
        };
        if let Some(last) = events.into_iter().last() {
            reading.path_event = Some((path.clone(), last));
        }
    }
    Ok(reading)
}

/// The job's `PATH` for a unit with no unit file whose drop-in files exist
/// (before the first install, or after a rollback or migration removed the unit):
/// systemd reads none of them now, but the install keeps and applies them, so
/// they are read here, in file-name order (a name in directories of different
/// names is ambiguous). Refuses what it cannot read as plain `Environment=PATH=` lines,
/// and every directive whose effect on `PATH` the manager cannot report for an
/// unloaded unit.
async fn path_from_orphan_drop_ins(
    manager_path: Option<String>,
    unit: &UnitEnvironment,
    unit_name: &str,
    reader: &mut UnitFileReader,
) -> Result<Option<String>, Error> {
    let refuse = |detail: String| Error::ManagerEnvironment { detail };
    let reading = read_drop_ins(unit, unit_name, reader).await?;
    if let Some((key, path)) = reading.path_filters.first() {
        return Err(refuse_path_filter(key, path, unit_name));
    }
    for (file, optional) in &reading.environment_files {
        check_environment_file(file, *optional, unit_name, reader).await?;
    }
    let current = reading.path_event.and_then(|(_, value)| value);
    let value = current
        .or_else(|| (!reading.search_path.is_empty()).then(|| reading.search_path.join(":")));
    match value {
        Some(value) => carry(&value).map(Some).map_err(refuse),
        None => Ok(manager_path),
    }
}

/// The `PATH` of a job whose unit file the install writes with
/// `Environment=PATH=<recorded>`, given the unit the manager reports: `recorded`
/// unless a drop-in replaces it, and the drop-in that did.
///
/// The drop-in files are read as a removed unit's are, which is also how a
/// recovery that removes the unit reads them, so every moment of an operation
/// judges the same value. For a loaded unit the value is checked against the
/// manager's merged `Environment`.
///
/// # Errors
///
/// Returns [`Error::ManagerEnvironment`] for the unit states and drop-in forms
/// [`effective_search_path`] refuses, an `Environment=` reset or
/// `UnsetEnvironment=` that removes the recorded `PATH`, and a drop-in value the
/// manager does not report.
pub async fn recorded_search_path(
    recorded: &str,
    unit: &UnitEnvironment,
    unit_name: &str,
    reader: &mut UnitFileReader,
) -> Result<RecordedPath, Error> {
    let refuse = |detail: String| Error::ManagerEnvironment { detail };
    let unchanged = || RecordedPath {
        value: recorded.to_owned(),
        drop_in: None,
    };
    let has_drop_ins = !unit.drop_in_paths.is_empty() || !unit.drop_ins_on_disk.is_empty();
    let loaded = unit.load_state == "loaded";
    if !has_drop_ins {
        // The install replaces the unit file, so nothing else sets the job's
        // environment. Only a unit in a state that cannot be judged is refused.
        if !loaded && unit.load_state != "not-found" {
            unit_can_set_path(unit, unit_name, reader, false).await?;
        }
        return Ok(unchanged());
    }
    if unit.load_state != "not-found" {
        // A loaded unit's stale snapshot, merged `UnsetEnvironment=` and
        // `EnvironmentFile=`; a masked or failed one is refused here.
        unit_can_set_path(unit, unit_name, reader, false).await?;
    }
    let reading = read_drop_ins(unit, unit_name, reader).await?;
    if let Some((key, path)) = reading
        .path_filters
        .iter()
        .find(|(key, _)| key == "UnsetEnvironment")
    {
        return Err(refuse_path_filter(key, path, unit_name));
    }
    for (file, optional) in &reading.environment_files {
        check_environment_file(file, *optional, unit_name, reader).await?;
    }
    match reading.path_event {
        None => Ok(unchanged()),
        Some((drop_in, None)) => Err(refuse(format!(
            "{unit_name}: the drop-in {drop_in} resets Environment=, which removes the {SEARCH_PATH_VARIABLE} that the unit file the install writes gives its job. Remove the empty `Environment=` line or set {SEARCH_PATH_VARIABLE} after it, and retry"
        ))),
        Some((drop_in, Some(value))) => {
            let value = carry(&value).map_err(refuse)?;
            let prefix = format!("{SEARCH_PATH_VARIABLE}=");
            let merged = unit
                .environment
                .iter()
                .rev()
                .find_map(|entry| entry.strip_prefix(prefix.as_str()));
            if loaded && merged != Some(value.as_str()) {
                return Err(refuse(format!(
                    "{unit_name}: the {SEARCH_PATH_VARIABLE} that the service manager reports for the unit differs from the one the drop-in {drop_in} sets, so what its job gets is not derived here. Write the value as a plain assignment and retry"
                )));
            }
            Ok(RecordedPath {
                value,
                drop_in: Some(drop_in),
            })
        }
    }
}

/// The `PATH` a job with a recorded one gets, and the drop-in that replaced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedPath {
    /// The job's `PATH`.
    pub value: String,
    /// The drop-in that sets `value` instead of the recorded one.
    pub drop_in: Option<String>,
}

/// Refuses a unit whose state the manager's answer cannot be trusted in: a
/// stale snapshot next to drop-ins, and a unit that is masked or failed to load.
fn unit_state_check(unit: &UnitEnvironment, unit_name: &str) -> Result<(), Error> {
    let refuse = |detail: String| Error::ManagerEnvironment { detail };
    // The unit file changing on disk is routine: the install replaces it. Only
    // drop-ins make a stale snapshot misleading, because they are kept.
    if unit.need_daemon_reload
        && (!unit.drop_in_paths.is_empty() || !unit.drop_ins_on_disk.is_empty())
    {
        return Err(refuse(format!(
            "{unit_name} or one of its drop-ins changed on disk after the service manager loaded it, so the PATH the manager reports may not be the one its job gets. Run `systemctl --user daemon-reload` and retry"
        )));
    }
    match unit.load_state.as_str() {
        "loaded" | "not-found" => Ok(()),
        other => Err(refuse(format!(
            "{unit_name} is {other}, so the PATH its job gets cannot be determined. Fix the unit (`systemctl --user status {unit_name}`) and retry"
        ))),
    }
}

/// Refuses the unit states and settings that make the job's `PATH` unreadable
/// here, and says whether the unit can give the job a `PATH` at all: `false` for
/// a unit that is not written yet.
///
/// `pass_environment_matters` is `false` for a job whose unit file sets `PATH`
/// itself, where `PassEnvironment=` has no effect because `Environment=` wins.
async fn unit_can_set_path(
    unit: &UnitEnvironment,
    unit_name: &str,
    reader: &mut UnitFileReader,
    pass_environment_matters: bool,
) -> Result<bool, Error> {
    let refuse = |detail: String| Error::ManagerEnvironment { detail };
    unit_state_check(unit, unit_name)?;
    if unit.load_state == "not-found"
        && unit.drop_in_paths.is_empty()
        && unit.drop_ins_on_disk.is_empty()
    {
        return Ok(false);
    }
    let names_path = |entry: &String| {
        entry == SEARCH_PATH_VARIABLE
            || entry
                .strip_prefix(SEARCH_PATH_VARIABLE)
                .is_some_and(|rest| rest.starts_with('='))
    };
    if pass_environment_matters
        && unit
            .pass_environment
            .iter()
            .any(|name| name == SEARCH_PATH_VARIABLE)
    {
        return Err(refuse(format!(
            "{unit_name} sets PassEnvironment= with {SEARCH_PATH_VARIABLE}, whose value is the service manager process's own and cannot be read here. Remove {SEARCH_PATH_VARIABLE} from PassEnvironment= in the unit or its drop-ins (`systemctl --user cat {unit_name}`) and retry"
        )));
    }
    if unit.unset_environment.iter().any(names_path) {
        return Err(refuse(format!(
            "{unit_name} sets UnsetEnvironment= for {SEARCH_PATH_VARIABLE}, so its job has no PATH to judge. Remove it from the unit or its drop-ins (`systemctl --user cat {unit_name}`) and retry"
        )));
    }
    for (file, optional) in &unit.environment_files {
        check_environment_file(file, *optional, unit_name, reader).await?;
    }
    Ok(true)
}

/// Reads a unit setting file: a regular file of at most
/// [`super::settings::UNIT_SETTINGS_FILE_LIMIT`] bytes, opened without blocking.
///
/// systemd follows symlinks here, so the CLI does too, but it never reads from a
/// FIFO or device and never reads more than the limit.
async fn read_unit_settings_file(
    path: &str,
    unit_name: &str,
    kind: &str,
) -> Result<std::io::Result<Vec<u8>>, Error> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let owned = path.to_owned();
    let read = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&owned)?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        let mut bytes = Vec::new();
        file.take(super::settings::UNIT_SETTINGS_FILE_LIMIT + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > super::settings::UNIT_SETTINGS_FILE_LIMIT {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "larger than the read limit",
            ));
        }
        Ok(bytes)
    })
    .await
    .map_err(|error| Error::ManagerEnvironment {
        detail: format!("reading the {kind} {path} of {unit_name} did not finish: {error}"),
    })?;
    Ok(read)
}

/// Reads a unit file or drop-in that must be readable, or refuses.
async fn read_unit_setting_file(path: &str, unit_name: &str, kind: &str) -> Result<Vec<u8>, Error> {
    read_unit_settings_file(path, unit_name, kind)
        .await?
        .map_err(|error| Error::ManagerEnvironment {
            detail: format!(
                "the {kind} {path} of {unit_name} cannot be read ({error}), so the PATH it may set cannot be judged. Make it a readable regular file of at most {} bytes and retry",
                super::settings::UNIT_SETTINGS_FILE_LIMIT
            ),
        })
}

/// Refuses an `EnvironmentFile=` that sets `PATH` or cannot be shown not to.
async fn check_environment_file(
    file: &str,
    optional: bool,
    unit_name: &str,
    reader: &mut UnitFileReader,
) -> Result<(), Error> {
    let keys = environment_file_keys(file, optional, unit_name, reader).await?;
    if keys
        .iter()
        .any(|key| key.trim_matches(SYSTEMD_WHITESPACE) == SEARCH_PATH_VARIABLE)
    {
        return Err(Error::ManagerEnvironment {
            detail: format!(
                "the EnvironmentFile= {file} of {unit_name} sets {SEARCH_PATH_VARIABLE}, and its quoting is not re-parsed here. Move the {SEARCH_PATH_VARIABLE} assignment to an Environment= line of a drop-in, or record the PATH in service.toml, and retry"
            ),
        });
    }
    Ok(())
}

/// The names an `EnvironmentFile=` assigns; none for an optional file that does
/// not exist. Refuses a file that cannot be listed or read, or whose quoting is
/// not followed, because what it assigns cannot then be told.
pub(super) async fn environment_file_keys(
    file: &str,
    optional: bool,
    unit_name: &str,
    reader: &mut UnitFileReader,
) -> Result<Vec<String>, Error> {
    let refuse = |detail: String| Error::ManagerEnvironment { detail };
    if file.contains(['*', '?', '[']) {
        return Err(refuse(format!(
            "{unit_name} names the EnvironmentFile= pattern {file}, whose files cannot be listed here. Replace it with plain paths in the unit or its drop-ins (`systemctl --user cat {unit_name}`) and retry"
        )));
    }
    let bytes = match reader.environment_file(file, unit_name).await? {
        Ok(bytes) => bytes,
        Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new())
        }
        Err(error) => {
            return Err(refuse(format!(
                "{unit_name} names the EnvironmentFile= {file}, which cannot be read ({error}), so its PATH cannot be judged. Make it a readable regular file of at most {} bytes, or prefix the path with `-` if it may be absent, and retry",
                super::settings::UNIT_SETTINGS_FILE_LIMIT
            )))
        }
    };
    environment_file_assigned_keys(&String::from_utf8_lossy(&bytes)).ok_or_else(|| {
        refuse(format!(
            "the EnvironmentFile= {file} of {unit_name} has quoting that is not followed here (a quote never closed, or another quote after a closed one in the same value), so whether it sets {SEARCH_PATH_VARIABLE} cannot be told. Use one quoted or unquoted value per line and retry"
        ))
    })
}

/// Whether `c` ends a line of an `EnvironmentFile=`; systemd ends it at `\n` and `\r`.
fn is_env_line_end(c: char) -> bool {
    c == '\n' || c == '\r'
}

/// The names an `EnvironmentFile=` assigns, or `None` when a quote is not closed
/// or quoting follows a closed quote.
///
/// Follows the file format: `KEY=VALUE` assignments, `#` and `;` comment lines,
/// values that may be quoted with `'` or `"` or continue after a trailing
/// backslash, so text inside such a value is never taken for a key.
fn environment_file_assigned_keys(text: &str) -> Option<Vec<String>> {
    let mut chars = text.chars().peekable();
    let mut keys: Vec<String> = Vec::new();
    loop {
        while chars.next_if(|c| SYSTEMD_WHITESPACE.contains(c)).is_some() {}
        let Some(first) = chars.peek().copied() else {
            return Some(keys);
        };
        if first == '#' || first == ';' {
            for c in chars.by_ref() {
                if is_env_line_end(c) {
                    break;
                }
            }
            continue;
        }
        let mut key = String::new();
        while let Some(c) = chars.next_if(|c| *c != '=' && !is_env_line_end(*c)) {
            key.push(c);
        }
        if chars.next_if_eq(&'=').is_none() {
            // A line without `=` assigns nothing.
            continue;
        }
        if key.trim_matches(SYSTEMD_WHITESPACE).is_empty() {
            // systemd drops an assignment without a name, and what it does with
            // a continuation after one is not followed here.
            return None;
        }
        keys.push(key.trim_matches(SYSTEMD_WHITESPACE).to_owned());
        // A value that opens with a quote ends at the matching one; quotes
        // anywhere else are literal. A trailing backslash continues the line.
        while chars.next_if(|c| *c == ' ' || *c == '\t').is_some() {}
        let mut closed_quote = false;
        if let Some(quote) = chars.next_if(|c| *c == '\'' || *c == '"') {
            closed_quote = true;
            loop {
                match chars.next() {
                    None => return None,
                    Some(c) if c == quote => break,
                    Some('\\') if quote == '"' => {
                        chars.next();
                    }
                    Some(_) => {}
                }
            }
        }
        loop {
            match chars.next() {
                None => return Some(keys),
                Some(c) if is_env_line_end(c) => break,
                Some('\\') => {
                    chars.next();
                }
                // systemd continues the value with another quoted segment here;
                // the scan does not follow that, so it gives up.
                Some('\'' | '"') if closed_quote => return None,
                Some(_) => {}
            }
        }
    }
}

/// The logical lines of a unit file, as systemd's reader sees them: a leading
/// UTF-8 byte order mark is dropped, a line ends at `\n`, `\r` or NUL, a
/// trailing run of an odd number of backslashes joins the next line, and comment
/// lines are skipped, also inside a continuation. A line holding bytes that are not UTF-8 is `Err`.
fn logical_lines(bytes: &[u8]) -> Vec<Result<String, String>> {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let mut lines = Vec::new();
    let mut current: Option<Vec<u8>> = None;
    for physical in physical_lines(bytes) {
        let trimmed = physical.trim_ascii_start();
        if trimmed.starts_with(b"#") || trimmed.starts_with(b";") {
            continue;
        }
        // A run of backslashes continues the line only when its length is odd;
        // an even run is escaped backslashes and the line ends.
        let backslashes = physical
            .iter()
            .rev()
            .take_while(|byte| **byte == b'\\')
            .count();
        let (content, continues) = if backslashes % 2 == 1 {
            (&physical[..physical.len() - 1], true)
        } else {
            (physical, false)
        };
        match current.as_mut() {
            Some(line) => {
                line.push(b' ');
                line.extend_from_slice(content);
            }
            None => current = Some(content.to_vec()),
        }
        if !continues {
            lines.extend(current.take());
        }
    }
    lines.extend(current);
    lines
        .into_iter()
        .map(|line| {
            let text = String::from_utf8(line)
                .map_err(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())?;
            // systemd also drops a line holding a Unicode noncharacter.
            if text.chars().any(is_noncharacter) {
                return Err(text);
            }
            Ok(text)
        })
        .collect()
}

/// Whether `c` is a Unicode noncharacter (U+FDD0..U+FDEF or U+xFFFE/U+xFFFF).
fn is_noncharacter(c: char) -> bool {
    let code = u32::from(c);
    (0xFDD0..=0xFDEF).contains(&code) || code & 0xFFFE == 0xFFFE
}

/// The physical lines of `bytes`: ended by `\n`, `\r\n`, a lone `\r` or NUL.
fn physical_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        let end = match bytes[index] {
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => 2,
            b'\n' | b'\r' | 0 => 1,
            _ => {
                index += 1;
                continue;
            }
        };
        lines.push(&bytes[start..index]);
        index += end;
        start = index;
    }
    lines.push(&bytes[start..]);
    lines
}

/// A setting of the unit's `[Service]` section that changes the job's `PATH`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Setting {
    /// An `Environment=` word that assigns `PATH`.
    EnvironmentPath,
    /// An `ExecSearchPath=` entry.
    ExecSearchPath,
}

impl Setting {
    /// The directive that carries the setting.
    fn directive(self) -> &'static str {
        match self {
            Self::EnvironmentPath => "Environment=PATH=",
            Self::ExecSearchPath => "ExecSearchPath=",
        }
    }

    /// The key of the unit file line.
    fn key(self) -> &'static str {
        match self {
            Self::EnvironmentPath => "Environment",
            Self::ExecSearchPath => "ExecSearchPath",
        }
    }
}

/// What a unit file or drop-in says about a [`Setting`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Assignment {
    /// A plain assignment systemd applies.
    Sets,
    /// No plain assignment, but a form that could still be one: an escape
    /// sequence, a specifier systemd drops, bytes that are not UTF-8 or a quote
    /// that never closes.
    Unclear,
    /// Nothing that can give the setting.
    No,
}

/// The specifier letters systemd resolves in a user unit's settings; `%%` is a
/// percent sign. An unknown one makes systemd drop the whole word.
const SPECIFIER_LETTERS: &str = "aAbBCdEfgGhHiIjJlLmMnNopPqsStTuUvVwWyY%";

/// Whether every `%` sequence of `word` is a specifier systemd resolves.
fn specifiers_resolve(word: &str) -> bool {
    let mut chars = word.chars();
    while let Some(c) = chars.next() {
        if c == '%'
            && !chars
                .next()
                .is_some_and(|next| SPECIFIER_LETTERS.contains(next))
        {
            return false;
        }
    }
    true
}

/// The characters systemd treats as white space in unit and environment files;
/// Unicode spaces such as U+00A0 are ordinary characters there.
const SYSTEMD_WHITESPACE: [char; 4] = [' ', '\t', '\n', '\r'];

/// The words of a unit file value, split and unquoted as systemd does.
struct Tokens {
    words: Vec<String>,
    /// A quote that never closes.
    open_quote: bool,
    /// An escape sequence, which is not decoded here.
    escaped: bool,
    /// An escape ahead of the first `=` of a word, which can spell a name.
    name_escaped: bool,
}

fn split_words(value: &str) -> Tokens {
    split_words_decoding(value, false)
}

/// [`split_words`] for an `Environment=` value, where an escaped backslash
/// (`\\\\`) is one backslash, as systemd reads it (pinned on the real manager);
/// every other escape sequence stays undecoded.
fn split_environment_words(value: &str) -> Tokens {
    split_words_decoding(value, true)
}

fn split_words_decoding(value: &str, decode_backslash: bool) -> Tokens {
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut out = Tokens {
        words: Vec::new(),
        open_quote: false,
        escaped: false,
        name_escaped: false,
    };
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match (c, quote) {
            ('\\', _) if decode_backslash && chars.as_str().starts_with('\\') => {
                word.push('\\');
                chars.next();
            }
            ('\\', _) => {
                out.escaped = true;
                out.name_escaped |= !word.contains('=');
                word.extend(chars.next());
            }
            ('\'' | '"', None) => quote = Some(c),
            (c, Some(q)) if c == q => quote = None,
            (c, None) if SYSTEMD_WHITESPACE.contains(&c) => {
                out.words.push(std::mem::take(&mut word));
            }
            (c, _) => word.push(c),
        }
    }
    out.words.push(word);
    out.words.retain(|word| !word.is_empty());
    out.open_quote = quote.is_some();
    out
}

/// Classifies `bytes`, a unit file or drop-in, for `setting` in its `[Service]`
/// section.
fn unit_setting(bytes: &[u8], setting: Setting) -> Assignment {
    let mut result = Assignment::No;
    for line in service_lines(bytes) {
        let line = match line {
            Ok(line) => line,
            Err(lossy) => {
                if lossy.contains(setting.key()) || lossy.contains(SEARCH_PATH_VARIABLE) {
                    result = Assignment::Unclear;
                }
                continue;
            }
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim_end_matches(SYSTEMD_WHITESPACE) != setting.key() {
            continue;
        }
        let split = match setting {
            Setting::EnvironmentPath => split_environment_words(value),
            Setting::ExecSearchPath => split_words(value),
        };
        let assignment = format!("{SEARCH_PATH_VARIABLE}=");
        let relevant = |word: &String| match setting {
            Setting::EnvironmentPath => word.starts_with(&assignment),
            Setting::ExecSearchPath => true,
        };
        let mentions = value.contains(SEARCH_PATH_VARIABLE)
            || (split.name_escaped && matches!(setting, Setting::EnvironmentPath))
            || matches!(setting, Setting::ExecSearchPath) && !split.words.is_empty();
        if split.open_quote || split.escaped {
            // systemd ignores an assignment whose quote never closes, and an
            // escape sequence is not decoded here.
            if mentions {
                result = Assignment::Unclear;
            }
            continue;
        }
        if split
            .words
            .iter()
            .any(|word| relevant(word) && specifiers_resolve(word))
        {
            return Assignment::Sets;
        }
        if split.words.iter().any(relevant) || mentions {
            // A word with a specifier systemd drops, or a lookalike name.
            result = Assignment::Unclear;
        }
    }
    result
}

/// The lines of the `[Service]` section of `bytes`, trimmed; a line that is not
/// UTF-8 is its lossy form as the error.
fn service_lines(bytes: &[u8]) -> Vec<Result<String, String>> {
    let mut in_service = false;
    let mut lines = Vec::new();
    for line in logical_lines(bytes) {
        let Ok(text) = &line else {
            lines.push(line);
            continue;
        };
        let trimmed = text.trim_matches(SYSTEMD_WHITESPACE);
        if let Some(section) = trimmed
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            in_service = section == "Service";
        } else if in_service {
            lines.push(Ok(trimmed.to_owned()));
        }
    }
    lines
}

/// What the `Environment=` lines of a drop-in do to `PATH`, in order: `None` for
/// a reset (an empty assignment), `Some(value)` for an assignment, specifiers
/// expanded. `Err(None)` is a line that may touch it in a form not read here,
/// `Err(Some(specifier))` a specifier that is not expanded here.
fn path_events(bytes: &[u8]) -> Result<Vec<Option<String>>, Option<String>> {
    let mut events = Vec::new();
    for line in service_lines(bytes) {
        let Ok(line) = line else {
            return Err(None);
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim_end_matches(SYSTEMD_WHITESPACE) != Setting::EnvironmentPath.key() {
            continue;
        }
        if value.trim_matches(SYSTEMD_WHITESPACE).is_empty() {
            events.push(None);
            continue;
        }
        let split = split_environment_words(value);
        if split.open_quote
            || split.name_escaped
            || (split.escaped && value.contains(SEARCH_PATH_VARIABLE))
        {
            return Err(None);
        }
        let assignment = format!("{SEARCH_PATH_VARIABLE}=");
        for word in &split.words {
            // A specifier in a name is expanded by systemd, so it can spell PATH.
            without_specifiers(word.split('=').next().unwrap_or_default()).map_err(Some)?;
            if let Some(path) = word.strip_prefix(&assignment) {
                if split.escaped {
                    return Err(None);
                }
                events.push(Some(without_specifiers(path).map_err(Some)?));
            }
        }
    }
    Ok(events)
}

/// What launchd reports for the daemon job's `PATH`, from both of its sources.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct ActiveJob {
    /// The domain's `PATH`, set with `launchctl setenv`, when the domain has one.
    pub domain: Option<String>,
    /// The `default environment` `PATH` of the loaded job, when it is loaded.
    pub loaded_default: Option<String>,
    /// The domain's `environment` variables, `PATH` included, in launchd's
    /// order; `None` when launchd has no such domain (no GUI session).
    pub domain_environment: Option<Vec<(String, String)>>,
}

// The domain's variables are printed by name only: their values may be secrets.
impl Debug for ActiveJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Option<Vec<&str>> = self
            .domain_environment
            .as_ref()
            .map(|entries| entries.iter().map(|(name, _)| name.as_str()).collect());
        f.debug_struct("ActiveJob")
            .field("domain", &self.domain)
            .field("loaded_default", &self.loaded_default)
            .field("domain_environment", &names)
            .finish_non_exhaustive()
    }
}

/// Reads launchd's active state of the daemon job.
pub trait LaunchdJob: Debug + Send + Sync {
    /// Returns the active `PATH` of job `label` of `gui/<uid>`, or the reason it
    /// cannot be read.
    fn active_path<'a>(
        &'a self,
        uid: u32,
        label: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ActiveJob, String>> + Send + 'a>>;

    /// Returns the variables of the domain `gui/<uid>`, `PATH` included, without
    /// naming any job. `None` when launchd has no such domain (no GUI session).
    /// Reads no state the install or upgrade has to create.
    fn domain_environment(&self, uid: u32) -> DomainEnvironment<'_>;

    /// Returns the current boot identity (`kern.bootsessionuuid`), or why it
    /// cannot be read. Two equal values mean no reboot happened between the
    /// reads, whatever the clock did.
    fn boot_identity(&self) -> Result<String, String>;

    /// Returns when the host booted, on the calendar clock the persisted
    /// configuration's modification time is on, or why it cannot be read.
    fn boot_time(&self) -> Result<std::time::SystemTime, String>;
}

/// A pending read of a launchd domain's variables, in launchd's order, or
/// `None` when the domain does not exist.
pub type DomainEnvironment<'a> =
    Pin<Box<dyn Future<Output = Result<Option<Vec<(String, String)>>, String>> + Send + 'a>>;

/// Reads the active state through `launchctl`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemLaunchd;

impl LaunchdJob for SystemLaunchd {
    #[cfg(target_os = "macos")]
    fn active_path<'a>(
        &'a self,
        uid: u32,
        label: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ActiveJob, String>> + Send + 'a>> {
        Box::pin(async move {
            let sources = pohunek_platform::supervisor::launchd::job_path_sources(
                uid,
                label,
                super::settings::LAUNCHCTL_COMMAND,
            )
            .await
            .map_err(|error| error.to_string())?;
            Ok(ActiveJob {
                domain: sources.domain,
                loaded_default: sources.loaded_default,
                domain_environment: sources.domain_environment,
            })
        })
    }

    /// launchd exists only on macOS.
    #[cfg(not(target_os = "macos"))]
    fn active_path<'a>(
        &'a self,
        _uid: u32,
        _label: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ActiveJob, String>> + Send + 'a>> {
        Box::pin(std::future::ready(Err(
            "launchd exists only on macOS".to_owned()
        )))
    }

    #[cfg(target_os = "macos")]
    fn domain_environment(&self, uid: u32) -> DomainEnvironment<'_> {
        Box::pin(async move {
            pohunek_platform::supervisor::launchd::domain_environment_of(
                uid,
                super::settings::LAUNCHCTL_COMMAND,
            )
            .await
            .map_err(|error| error.to_string())
        })
    }

    /// launchd exists only on macOS.
    #[cfg(not(target_os = "macos"))]
    fn domain_environment(&self, _uid: u32) -> DomainEnvironment<'_> {
        Box::pin(std::future::ready(Err(
            "launchd exists only on macOS".to_owned()
        )))
    }

    #[cfg(target_os = "macos")]
    fn boot_identity(&self) -> Result<String, String> {
        pohunek_platform::process::DarwinInspector::new()
            .boot_session_uuid()
            .map_err(|error| error.to_string())
    }

    /// The boot identity of a launchd host exists only on macOS.
    #[cfg(not(target_os = "macos"))]
    fn boot_identity(&self) -> Result<String, String> {
        Err("launchd exists only on macOS".to_owned())
    }

    #[cfg(target_os = "macos")]
    fn boot_time(&self) -> Result<std::time::SystemTime, String> {
        pohunek_platform::process::DarwinInspector::new()
            .boot_time()
            .map_err(|error| error.to_string())
    }

    /// The boot time of a launchd host exists only on macOS.
    #[cfg(not(target_os = "macos"))]
    fn boot_time(&self) -> Result<std::time::SystemTime, String> {
        Err("launchd exists only on macOS".to_owned())
    }
}
