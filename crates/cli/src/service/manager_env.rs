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
    /// The manager's own `HOME`, which its `%h` specifier expands to.
    pub home: Option<String>,
    /// The manager's own `XDG_RUNTIME_DIR`, which its `%t` specifier expands to.
    pub runtime_dir: Option<String>,
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
    let find = |name: &str| {
        entries
            .iter()
            .find_map(|entry| entry.strip_prefix(name)?.strip_prefix('='))
            .map(str::to_owned)
    };
    Ok(UserManagerEnvironment {
        search_path,
        variables: allowlisted_variables(pairs, allowlist)?,
        home: find("HOME"),
        runtime_dir: find("XDG_RUNTIME_DIR"),
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

/// The values of the specifiers a drop-in of a removed unit may use. systemd
/// expands them in a loaded unit; for an unloaded one they are expanded here,
/// and only those whose value does not depend on the manager.
#[derive(Debug, Clone, Default)]
pub struct Specifiers {
    /// `%h`, the user's home directory; `None` when the manager's `HOME` cannot be
    /// confirmed equal to it.
    pub home: Option<String>,
    /// `%U`, the numeric user ID.
    pub uid: u32,
    /// `%t`, the runtime directory; `None` when the manager's `XDG_RUNTIME_DIR`
    /// cannot be confirmed equal to it.
    pub runtime_dir: Option<String>,
}

impl Specifiers {
    /// Expands `%%`, `%h`, `%U` and `%t` in `text`; any other specifier, or one
    /// without a value, is the `Err` naming it.
    fn expand(&self, text: &str) -> Result<String, String> {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('%') => out.push('%'),
                Some('h') if self.home.is_some() => {
                    out.push_str(self.home.as_deref().unwrap_or_default());
                }
                Some('t') if self.runtime_dir.is_some() => {
                    out.push_str(self.runtime_dir.as_deref().unwrap_or_default());
                }
                Some('U') => out.push_str(&self.uid.to_string()),
                Some(other) => return Err(format!("%{other}")),
                None => return Err("%".to_owned()),
            }
        }
        Ok(out)
    }
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
    specifiers: &Specifiers,
) -> Result<Option<String>, Error> {
    if unit.load_state == "not-found"
        && (!unit.drop_in_paths.is_empty() || !unit.drop_ins_on_disk.is_empty())
    {
        return path_from_orphan_drop_ins(manager_path, unit, unit_name, specifiers).await;
    }
    if !unit_can_set_path(unit, unit_name).await? {
        return Ok(manager_path);
    }
    let loaded = loaded_search_path(manager_path.clone(), unit, unit_name).await?;
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
        let after =
            path_from_orphan_drop_ins(manager_path, &removed, unit_name, specifiers).await?;
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

/// The job's `PATH` from the manager's merged properties of a loaded unit.
async fn loaded_search_path(
    manager_path: Option<String>,
    unit: &UnitEnvironment,
    unit_name: &str,
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
        read_unit_setting_file(&unit.fragment_path, unit_name, "unit file").await?
    };
    let mut drop_ins = Vec::with_capacity(unit.drop_in_paths.len());
    for path in &unit.drop_in_paths {
        drop_ins.push((
            path,
            read_unit_setting_file(path, unit_name, "drop-in").await?,
        ));
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

/// Reads a drop-in, or `None` for one linked to `/dev/null`, which disables the
/// drop-in of that name.
async fn read_enabled_drop_in(path: &str, unit_name: &str) -> Result<Option<Vec<u8>>, Error> {
    if std::fs::canonicalize(path).is_ok_and(|target| target == std::path::Path::new("/dev/null")) {
        return Ok(None);
    }
    read_unit_setting_file(path, unit_name, "drop-in")
        .await
        .map(Some)
}

/// Orders `files` by file name, the order systemd applies drop-ins in, and
/// refuses a name that two directories share: which one applies is not derived.
fn refuse_shared_names(files: &mut [String], unit_name: &str) -> Result<(), Error> {
    let name_of = |path: &String| path.rsplit('/').next().unwrap_or(path).to_owned();
    files.sort_by_key(name_of);
    for pair in files.windows(2) {
        if name_of(&pair[0]) == name_of(&pair[1]) {
            return Err(Error::ManagerEnvironment {
                detail: format!(
                    "{unit_name} has no unit file yet and the drop-in {} exists in two directories ({} and {}), so which one systemd applies is not derived here. Remove one of them and retry",
                    name_of(&pair[0]),
                    pair[0],
                    pair[1]
                ),
            });
        }
    }
    Ok(())
}

/// The job's `PATH` for a unit with no unit file whose drop-in files exist
/// (before the first install, or after a rollback or migration removed the unit):
/// systemd reads none of them now, but the install keeps and applies them, so
/// they are read here, in file-name order (a name in two directories is
/// ambiguous). Refuses what it cannot read as plain `Environment=PATH=` lines,
/// and every directive whose effect on `PATH` the manager cannot report for an
/// unloaded unit.
async fn path_from_orphan_drop_ins(
    manager_path: Option<String>,
    unit: &UnitEnvironment,
    unit_name: &str,
    specifiers: &Specifiers,
) -> Result<Option<String>, Error> {
    let refuse = |detail: String| Error::ManagerEnvironment { detail };
    let mut files = unit.drop_in_paths.clone();
    files.extend(unit.drop_ins_on_disk.iter().cloned());
    files.sort();
    files.dedup();
    refuse_shared_names(&mut files, unit_name)?;
    let mut current: Option<String> = None;
    let mut search_path: Vec<String> = Vec::new();
    let mut environment_files: Vec<(String, bool)> = Vec::new();
    for path in &files {
        let Some(bytes) = read_enabled_drop_in(path, unit_name).await? else {
            continue;
        };
        let unreadable = |directive: &str| {
            refuse(format!(
                "{unit_name} has no unit file yet and the drop-in {path} has a {directive} line that may affect {SEARCH_PATH_VARIABLE} in a form not read here (an escape sequence, more than one word, a quote never closed, or bytes that are not UTF-8). Write a plain assignment and retry"
            ))
        };
        let unsupported = |directive: &str, specifier: &str| {
            refuse(format!(
                "{unit_name} has no unit file yet and the drop-in {path} uses the specifier {specifier} in {directive}=, which is not expanded here (only %U and %% always are; %h and %t only while the service manager's own HOME and XDG_RUNTIME_DIR agree with the CLI's, which cannot be told for this unit). Write the value out and retry"
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
                            environment_files.clear();
                        } else {
                            search_path.clear();
                        }
                        continue;
                    }
                    let split = split_words(value);
                    if split.open_quote
                        || split.escaped
                        || (key == "ExecSearchPath" && split.words.len() != 1)
                    {
                        return Err(unreadable(key));
                    }
                    if key == "EnvironmentFile" {
                        // The whole value is one path, spaces included.
                        let raw = value.trim_matches(SYSTEMD_WHITESPACE);
                        if raw.contains(['"', '\'']) {
                            return Err(unreadable(key));
                        }
                        let raw = specifiers
                            .expand(raw)
                            .map_err(|specifier| unsupported(key, &specifier))?;
                        let optional = raw.starts_with('-');
                        let file = raw.strip_prefix('-').unwrap_or(&raw).to_owned();
                        environment_files.push((file, optional));
                    } else {
                        let list = specifiers
                            .expand(&split.words[0])
                            .map_err(|specifier| unsupported(key, &specifier))?;
                        for entry in list.split(':').filter(|entry| !entry.is_empty()) {
                            if !entry.starts_with('/') {
                                return Err(unreadable(key));
                            }
                            search_path.push(entry.to_owned());
                        }
                    }
                }
                "PassEnvironment" | "UnsetEnvironment"
                    if value.contains(SEARCH_PATH_VARIABLE) || value.contains('\\') =>
                {
                    return Err(refuse(format!(
                        "{unit_name} has no unit file yet and the drop-in {path} sets {key}= for {SEARCH_PATH_VARIABLE}, which the service manager cannot report for an unloaded unit. Remove that line and retry"
                    )));
                }
                _ => {}
            }
        }
        let events = match path_events(&bytes, specifiers) {
            Ok(events) => events,
            Err(Some(specifier)) => return Err(unsupported("Environment", &specifier)),
            Err(None) => return Err(unreadable("Environment=")),
        };
        if let Some(last) = events.into_iter().last() {
            current = last;
        }
    }
    for (file, optional) in &environment_files {
        check_environment_file(file, *optional, unit_name).await?;
    }
    let value = current.or_else(|| (!search_path.is_empty()).then(|| search_path.join(":")));
    match value {
        Some(value) => carry(&value).map(Some).map_err(refuse),
        None => Ok(manager_path),
    }
}

/// Refuses the unit states and settings that make the job's `PATH` unreadable
/// here, and says whether the unit can give the job a `PATH` at all: `false` for
/// a unit that is not written yet.
async fn unit_can_set_path(unit: &UnitEnvironment, unit_name: &str) -> Result<bool, Error> {
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
        "loaded" => {}
        "not-found" if unit.drop_in_paths.is_empty() && unit.drop_ins_on_disk.is_empty() => {
            return Ok(false);
        }
        other => {
            return Err(refuse(format!(
                "{unit_name} is {other}, so the PATH its job gets cannot be determined. Fix the unit (`systemctl --user status {unit_name}`) and retry"
            )))
        }
    }
    let names_path = |entry: &String| {
        entry == SEARCH_PATH_VARIABLE
            || entry
                .strip_prefix(SEARCH_PATH_VARIABLE)
                .is_some_and(|rest| rest.starts_with('='))
    };
    if unit
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
        check_environment_file(file, *optional, unit_name).await?;
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
async fn check_environment_file(file: &str, optional: bool, unit_name: &str) -> Result<(), Error> {
    let refuse = |detail: String| Error::ManagerEnvironment { detail };
    if file.contains(['*', '?', '[']) {
        return Err(refuse(format!(
            "{unit_name} names the EnvironmentFile= pattern {file}, whose files cannot be listed here. Replace it with plain paths in the unit or its drop-ins (`systemctl --user cat {unit_name}`) and retry"
        )));
    }
    let bytes = match read_unit_settings_file(file, unit_name, "EnvironmentFile=").await? {
        Ok(bytes) => bytes,
        Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(refuse(format!(
                "{unit_name} names the EnvironmentFile= {file}, which cannot be read ({error}), so its PATH cannot be judged. Make it a readable regular file of at most {} bytes, or prefix the path with `-` if it may be absent, and retry",
                super::settings::UNIT_SETTINGS_FILE_LIMIT
            )))
        }
    };
    match environment_file_sets_path(&String::from_utf8_lossy(&bytes)) {
        Some(false) => Ok(()),
        Some(true) => Err(refuse(format!(
            "the EnvironmentFile= {file} of {unit_name} sets {SEARCH_PATH_VARIABLE}, and its quoting is not re-parsed here. Move the {SEARCH_PATH_VARIABLE} assignment to an Environment= line of a drop-in, or record the PATH in service.toml, and retry"
        ))),
        None => Err(refuse(format!(
            "the EnvironmentFile= {file} of {unit_name} has quoting that is not followed here (a quote never closed, or another quote after a closed one in the same value), so whether it sets {SEARCH_PATH_VARIABLE} cannot be told. Use one quoted or unquoted value per line and retry"
        ))),
    }
}

/// Whether `c` ends a line of an `EnvironmentFile=`; systemd ends it at `\n` and `\r`.
fn is_env_line_end(c: char) -> bool {
    c == '\n' || c == '\r'
}

/// Whether an `EnvironmentFile=` assigns `PATH`, or `None` when a quote is not
/// closed or quoting follows a closed quote.
///
/// Follows the file format: `KEY=VALUE` assignments, `#` and `;` comment lines,
/// values that may be quoted with `'` or `"` or continue after a trailing
/// backslash, so text inside such a value is never taken for a key.
fn environment_file_sets_path(text: &str) -> Option<bool> {
    let mut chars = text.chars().peekable();
    let mut sets_path = false;
    loop {
        while chars.next_if(|c| SYSTEMD_WHITESPACE.contains(c)).is_some() {}
        let Some(first) = chars.peek().copied() else {
            return Some(sets_path);
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
        if key.trim_matches(SYSTEMD_WHITESPACE) == SEARCH_PATH_VARIABLE {
            sets_path = true;
        }
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
                None => return Some(sets_path),
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
/// trailing backslash joins the next line, and comment lines are skipped, also
/// inside a continuation. A line holding bytes that are not UTF-8 is `Err`.
fn logical_lines(bytes: &[u8]) -> Vec<Result<String, String>> {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let mut lines = Vec::new();
    let mut current: Option<Vec<u8>> = None;
    for physical in physical_lines(bytes) {
        let trimmed = physical.trim_ascii_start();
        if trimmed.starts_with(b"#") || trimmed.starts_with(b";") {
            continue;
        }
        let (content, continues) = match physical.strip_suffix(b"\\") {
            Some(content) => (content, true),
            None => (physical, false),
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
            String::from_utf8(line)
                .map_err(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
        })
        .collect()
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
        let split = split_words(value);
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
fn path_events(
    bytes: &[u8],
    specifiers: &Specifiers,
) -> Result<Vec<Option<String>>, Option<String>> {
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
        let split = split_words(value);
        if split.open_quote
            || split.name_escaped
            || (split.escaped && value.contains(SEARCH_PATH_VARIABLE))
        {
            return Err(None);
        }
        let assignment = format!("{SEARCH_PATH_VARIABLE}=");
        for word in &split.words {
            if let Some(path) = word.strip_prefix(&assignment) {
                if split.escaped {
                    return Err(None);
                }
                events.push(Some(specifiers.expand(path).map_err(Some)?));
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
}
