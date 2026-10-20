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

/// The `PATH` the daemon job gets from the user manager and its own unit.
///
/// `manager_path` is the manager's `PATH`; `unit` is what the manager holds for
/// the daemon unit (`unit_name`), drop-ins merged. The install replaces the unit
/// file itself and keeps its drop-ins, so only an `Environment=` entry for `PATH`
/// that a drop-in sets applies to the job; the last one wins. Every setting that
/// could change `PATH` but whose result is not derivable here is refused, with
/// the cause and the directive to change, so the caller never judges a `PATH` the
/// job does not get. Each `EnvironmentFile=` and drop-in is read once, bounded;
/// values are never part of an error.
///
/// # Errors
///
/// Returns [`Error::ManagerEnvironment`] for a unit that is masked or failed to
/// load, a drop-in on a unit with no file, an `EnvironmentFile=` that sets
/// `PATH`, cannot be read, is not a regular file or is a wildcard, a drop-in that
/// cannot be read, and `PassEnvironment=` or `UnsetEnvironment=` naming `PATH`.
pub async fn effective_search_path(
    manager_path: Option<String>,
    unit: &UnitEnvironment,
    unit_name: &str,
) -> Result<Option<String>, Error> {
    let refuse = |detail: String| Error::ManagerEnvironment { detail };
    if unit.need_daemon_reload {
        return Err(refuse(format!(
            "{unit_name} or one of its drop-ins changed on disk after the service manager loaded it, so the PATH the manager reports may not be the one its job gets. Run `systemctl --user daemon-reload` and retry"
        )));
    }
    match unit.load_state.as_str() {
        "loaded" => {}
        "not-found" if unit.drop_in_paths.is_empty() => return Ok(manager_path),
        "not-found" => {
            return Err(refuse(format!(
                "{unit_name} has drop-in files ({}) but no unit file yet, so the PATH they set cannot be read before the unit is written. Remove the drop-ins, or install the unit first and retry",
                unit.drop_in_paths.join(", ")
            )))
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
    let prefix = format!("{SEARCH_PATH_VARIABLE}=");
    let merged = unit
        .environment
        .iter()
        .rev()
        .find_map(|entry| entry.strip_prefix(prefix.as_str()));
    let Some(merged) = merged else {
        return Ok(manager_path);
    };
    // The merged list also holds the unit file's own entries, which the install
    // replaces; the entry applies only when a drop-in sets `PATH`.
    let mut unclear: Option<&str> = None;
    for drop_in in &unit.drop_in_paths {
        let text = read_unit_settings_file(drop_in, unit_name, "drop-in")
            .await?
            .map_err(|error| {
                refuse(format!(
                    "the drop-in {drop_in} of {unit_name} cannot be read ({error}), so the PATH it may set cannot be judged. Make it a readable regular file of at most {} bytes and retry",
                    super::settings::UNIT_SETTINGS_FILE_LIMIT
                ))
            })?;
        match drop_in_path_assignment(&text) {
            PathAssignment::Sets => return carry(merged).map(Some).map_err(refuse),
            PathAssignment::Unclear => unclear = Some(drop_in),
            PathAssignment::No => {}
        }
    }
    if let Some(drop_in) = unclear {
        return Err(refuse(format!(
            "the drop-in {drop_in} of {unit_name} has an Environment= line that may set {SEARCH_PATH_VARIABLE} in a form not read here (an escape sequence or a name containing {SEARCH_PATH_VARIABLE}), while {unit_name} also gets {SEARCH_PATH_VARIABLE} from its unit file, which the install replaces. Write it as a plain `Environment={SEARCH_PATH_VARIABLE}=...` assignment or remove the other {SEARCH_PATH_VARIABLE} entry, and retry"
        )));
    }
    Ok(manager_path)
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
) -> Result<std::io::Result<String>, Error> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let owned = path.to_owned();
    let read = tokio::task::spawn_blocking(move || -> std::io::Result<String> {
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
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    })
    .await
    .map_err(|error| Error::ManagerEnvironment {
        detail: format!("reading the {kind} {path} of {unit_name} did not finish: {error}"),
    })?;
    Ok(read)
}

/// Refuses an `EnvironmentFile=` that sets `PATH` or cannot be shown not to.
async fn check_environment_file(file: &str, optional: bool, unit_name: &str) -> Result<(), Error> {
    let refuse = |detail: String| Error::ManagerEnvironment { detail };
    if file.contains(['*', '?', '[']) {
        return Err(refuse(format!(
            "{unit_name} names the EnvironmentFile= pattern {file}, whose files cannot be listed here. Replace it with plain paths in the unit or its drop-ins (`systemctl --user cat {unit_name}`) and retry"
        )));
    }
    let text = match read_unit_settings_file(file, unit_name, "EnvironmentFile=").await? {
        Ok(text) => text,
        Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(refuse(format!(
                "{unit_name} names the EnvironmentFile= {file}, which cannot be read ({error}), so its PATH cannot be judged. Make it a readable regular file of at most {} bytes, or prefix the path with `-` if it may be absent, and retry",
                super::settings::UNIT_SETTINGS_FILE_LIMIT
            )))
        }
    };
    match environment_file_sets_path(&text) {
        Some(false) => Ok(()),
        Some(true) => Err(refuse(format!(
            "the EnvironmentFile= {file} of {unit_name} sets {SEARCH_PATH_VARIABLE}, and its quoting is not re-parsed here. Move the {SEARCH_PATH_VARIABLE} assignment to an Environment= line of a drop-in, or record the PATH in service.toml, and retry"
        ))),
        None => Err(refuse(format!(
            "the EnvironmentFile= {file} of {unit_name} has an unterminated quote, so whether it sets {SEARCH_PATH_VARIABLE} cannot be told. Fix the file and retry"
        ))),
    }
}

/// Whether an `EnvironmentFile=` assigns `PATH`, or `None` when a quote is not
/// closed.
///
/// Follows the file format: `KEY=VALUE` assignments, `#` and `;` comment lines,
/// values that may be quoted with `'` or `"` or continue after a trailing
/// backslash, so text inside such a value is never taken for a key.
fn environment_file_sets_path(text: &str) -> Option<bool> {
    let mut chars = text.chars().peekable();
    let mut sets_path = false;
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let Some(first) = chars.peek().copied() else {
            return Some(sets_path);
        };
        if first == '#' || first == ';' {
            for c in chars.by_ref() {
                if c == '\n' {
                    break;
                }
            }
            continue;
        }
        let mut key = String::new();
        while let Some(c) = chars.next_if(|c| *c != '=' && *c != '\n') {
            key.push(c);
        }
        if chars.next_if_eq(&'=').is_none() {
            // A line without `=` assigns nothing.
            continue;
        }
        if key.trim() == SEARCH_PATH_VARIABLE {
            sets_path = true;
        }
        // A value that opens with a quote ends at the matching one; quotes
        // anywhere else are literal. A trailing backslash continues the line.
        while chars.next_if(|c| *c == ' ' || *c == '\t').is_some() {}
        if let Some(quote) = chars.next_if(|c| *c == '\'' || *c == '"') {
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
                Some('\n') => break,
                Some('\\') => {
                    chars.next();
                }
                Some(_) => {}
            }
        }
    }
}

/// The logical lines of a unit file: a trailing backslash joins the next line,
/// and comment lines are skipped, also inside a continuation.
fn logical_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current: Option<String> = None;
    for physical in text.lines() {
        let trimmed = physical.trim_start();
        if trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        let (content, continues) = match physical.strip_suffix('\\') {
            Some(content) => (content, true),
            None => (physical, false),
        };
        match current.as_mut() {
            Some(line) => {
                line.push(' ');
                line.push_str(content);
            }
            None => current = Some(content.to_owned()),
        }
        if !continues {
            lines.extend(current.take());
        }
    }
    lines.extend(current);
    lines
}

/// What a unit drop-in says about `PATH` in its `Environment=` lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathAssignment {
    /// A plain `PATH=` assignment.
    Sets,
    /// No plain assignment, but an escape sequence or a name containing `PATH`
    /// that could still decode to one.
    Unclear,
    /// Nothing that can set `PATH`.
    No,
}

/// Classifies the `Environment=` lines of a unit drop-in.
///
/// Joins continuation lines, skips comment lines, reads only the `[Service]`
/// section, and splits each value into words outside quotes, as the unit file
/// syntax does.
fn drop_in_path_assignment(text: &str) -> PathAssignment {
    let mut result = PathAssignment::No;
    let mut in_service = false;
    for line in logical_lines(text) {
        let trimmed = line.trim();
        if let Some(section) = trimmed
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            in_service = section == "Service";
            continue;
        }
        if !in_service {
            continue;
        }
        let Some((key, value)) = line.trim_start().split_once('=') else {
            continue;
        };
        if key.trim_end() != "Environment" {
            continue;
        }
        let mut word = String::new();
        let mut quote: Option<char> = None;
        let mut words = Vec::new();
        let mut escaped = false;
        let mut chars = value.chars();
        while let Some(c) = chars.next() {
            match (c, quote) {
                ('\\', _) => {
                    escaped = true;
                    word.extend(chars.next());
                }
                ('\'' | '"', None) => quote = Some(c),
                (c, Some(q)) if c == q => quote = None,
                (c, None) if c.is_whitespace() => words.push(std::mem::take(&mut word)),
                (c, _) => word.push(c),
            }
        }
        words.push(word);
        let assignment = format!("{SEARCH_PATH_VARIABLE}=");
        if !escaped && words.iter().any(|word| word.starts_with(&assignment)) {
            return PathAssignment::Sets;
        }
        if escaped || value.contains(SEARCH_PATH_VARIABLE) {
            result = PathAssignment::Unclear;
        }
    }
    result
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

    /// Returns the current boot identity (`kern.boottime`), or why it cannot be
    /// read. Two equal values mean no reboot happened between the reads.
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
            .boot_identity()
            .map(|identity| identity.as_str().to_owned())
            .map_err(|error| error.to_string())
    }

    /// The boot identity of a launchd host exists only on macOS.
    #[cfg(not(target_os = "macos"))]
    fn boot_identity(&self) -> Result<String, String> {
        Err("launchd exists only on macOS".to_owned())
    }
}
