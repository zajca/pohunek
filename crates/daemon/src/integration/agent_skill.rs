//! The opt-in install of the bundled agent skill into an agent's skill
//! location.
//!
//! The runtime package declares where its agent reads user-level skills
//! (`[agent_skill]`); core owns the content, the file layout and every write:
//! `<skills dir>/pohunek/SKILL.md` plus an ownership marker
//! `<skills dir>/pohunek/.pohunek-managed`. `SKILL.md` is byte-identical to
//! `pohunek agent-skill`, so the marker carries the ownership proof beside it.
//! Every path component below the agent's root is opened without following
//! links, must be owned by the daemon user and must not be group- or
//! world-writable; anything else is reported as `unsafe` and never followed.

// Rust guideline compliant 2026-11-11

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use knowledge::{agent_skill_sha256, AGENT_SKILL};
use pohunek_platform::filesystem::{FsError, TrustedDir};
use protocol::{
    AgentSkillChange, AgentSkillRemoval, AgentSkillReport, AgentSkillState, AgentSkillStatus,
    ErrorClass, ProtocolError,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tracing::warn;

use super::homes::{HomeSelection, Target};

/// Name of the skill and of its directory below the skills directory.
const SKILL_NAME: &str = "pohunek";

/// File name of the skill inside its directory.
const SKILL_FILE: &str = "SKILL.md";

/// File name of the ownership marker inside the skill directory.
const MARKER_FILE: &str = ".pohunek-managed";

/// Version of the marker document.
const MARKER_SCHEMA: u32 = 1;

/// Mode of the skill file and its marker.
const FILE_MODE: u32 = 0o600;

/// Mode of a directory this module creates.
const DIR_MODE: u32 = 0o700;

/// Permission bits that let another account modify an entry.
const OTHERS_WRITE_BITS: u32 = 0o022;

/// Permission bits of a file mode.
const MODE_BITS: u32 = 0o7777;

/// Largest skill file read, in bytes. The skill is a few kilobytes; the bound
/// keeps a runaway file from exhausting memory.
const MAX_SKILL_BYTES: usize = 1 << 20;

/// Largest marker read, in bytes.
const MAX_MARKER_BYTES: usize = 4096;

/// Most entries listed in the skill directory.
const MAX_ENTRIES: usize = 64;

/// Random bytes in a temporary file name.
const TEMPORARY_RANDOM_BYTES: usize = 8;

/// Prefix of the temporary file a refresh stages.
const TEMPORARY_PREFIX: &str = ".pohunek-skill-";

/// Error code: the runtime declares no skill location.
pub(crate) const UNSUPPORTED_CODE: &str = "integration_agent_skill_unsupported";

/// Error code: a skill of that name exists that this installer does not own.
const FOREIGN_CODE: &str = "integration_agent_skill_foreign";

/// Error code: the owned skill file was edited.
const MODIFIED_CODE: &str = "integration_agent_skill_modified";

/// Error code: the skill location is a link, foreign-owned or writable by
/// others, or could not be written safely.
const UNSAFE_CODE: &str = "integration_agent_skill_unsafe";

/// The ownership marker document.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Marker {
    schema: u32,
    skill: String,
    sha256: String,
}

impl Marker {
    fn for_content(sha256: String) -> Self {
        Self {
            schema: MARKER_SCHEMA,
            skill: SKILL_NAME.to_owned(),
            sha256,
        }
    }

    fn parse(bytes: &[u8]) -> Option<Self> {
        let marker: Self = serde_json::from_slice(bytes).ok()?;
        let hex = marker.sha256.len() == 64
            && marker
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        (marker.schema == MARKER_SCHEMA && marker.skill == SKILL_NAME && hex).then_some(marker)
    }

    fn render(&self) -> Vec<u8> {
        let mut text = serde_json::to_string(self).unwrap_or_default();
        text.push('\n');
        text.into_bytes()
    }
}

/// The skill location of one home.
struct Site {
    /// The directory the agent's root is joined with the declared `dir` to.
    skills_dir: PathBuf,
    /// The agent's root: the config home or the user's home.
    root: PathBuf,
    /// The declared directory components below the root.
    components: Vec<String>,
    /// Whether every home of the agent shares the location.
    shared: bool,
}

impl Site {
    fn skill_path(&self) -> PathBuf {
        self.skills_dir.join(SKILL_NAME).join(SKILL_FILE)
    }
}

/// The site of `target`; `None` when its runtime declares no location, the
/// resolution error when the location does not resolve.
fn site_of(target: &Target) -> Option<Result<Site, ProtocolError>> {
    let declared = target.resolved.agent_skill.as_ref()?;
    let skills_dir = match target.skill_dir.as_ref()? {
        Ok(dir) => dir.clone(),
        Err(error) => return Some(Err(error.clone())),
    };
    let components: Vec<String> = declared.dir().split('/').map(str::to_owned).collect();
    let root = skills_dir
        .ancestors()
        .nth(components.len())
        .map(Path::to_path_buf);
    Some(
        root.map(|root| Site {
            skills_dir,
            root,
            components,
            shared: declared.is_shared(),
        })
        .ok_or_else(|| unsafe_error("the skills directory has no root", None)),
    )
}

fn error(code: &'static str, message: String, recover: String) -> ProtocolError {
    ProtocolError::new(ErrorClass::Configuration, code, message, Some(recover))
}

fn unsafe_error(reason: &str, path: Option<&Path>) -> ProtocolError {
    let shown = path.map_or_else(String::new, |path| format!(" ({})", path.display()));
    error(
        UNSAFE_CODE,
        format!("the agent skill location is not safe to write: {reason}{shown}"),
        "make every directory on the path a real directory owned by you and writable by you only"
            .to_owned(),
    )
}

/// What the skill location holds.
struct Inspection {
    state: AgentSkillState,
    installed_sha256: Option<String>,
    /// Mode of the skill file as found, for the compare before a refresh.
    skill_mode: Option<u32>,
}

impl Inspection {
    fn of(state: AgentSkillState) -> Self {
        Self {
            state,
            installed_sha256: None,
            skill_mode: None,
        }
    }
}

/// The opened directories of a skill location.
struct Chain {
    /// The skills directory.
    parent: TrustedDir,
    /// The `pohunek` skill directory.
    skill: TrustedDir,
}

/// Why a skill location could not be opened.
enum OpenFault {
    /// The path is unsafe or could not be inspected.
    Unsafe(String),
    /// The agent's root directory does not exist.
    RootMissing,
}

fn not_found(error: &FsError) -> bool {
    error.io_kind() == Some(std::io::ErrorKind::NotFound)
}

/// Opens the skill directory without following links; `None` when it does not
/// exist and `create` is not set.
fn open_chain(site: &Site, create: bool) -> Result<Option<Chain>, OpenFault> {
    let unsafe_dir = |error: FsError| OpenFault::Unsafe(error.to_string());
    let root = match (site.root.parent(), site.root.file_name()) {
        (Some(parent), Some(name)) => fs::canonicalize(parent)
            .map(|parent| parent.join(name))
            .map_err(|_error| OpenFault::RootMissing)?,
        _ => return Err(OpenFault::Unsafe("the agent root has no parent".to_owned())),
    };
    let mut directory = match TrustedDir::open_absolute_owner_safe(&root, OTHERS_WRITE_BITS) {
        Ok(directory) => directory,
        Err(error) if not_found(&error) => return Err(OpenFault::RootMissing),
        Err(error) => return Err(unsafe_dir(error)),
    };
    let names = site
        .components
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(SKILL_NAME));
    let mut parent = None;
    for name in names {
        let next = match directory.open_child_owner_safe(name, OTHERS_WRITE_BITS) {
            Ok(next) => next,
            Err(error) if not_found(&error) => {
                if !create {
                    return Ok(None);
                }
                directory
                    .create_child_exclusive(name, DIR_MODE)
                    .map_err(unsafe_dir)?
            }
            Err(error) => return Err(unsafe_dir(error)),
        };
        parent = Some(std::mem::replace(&mut directory, next));
    }
    Ok(parent.map(|parent| Chain {
        parent,
        skill: directory,
    }))
}

/// A file inside the skill directory.
enum Entry {
    Missing,
    /// A regular file with its content (when readable) and mode.
    File {
        bytes: Option<Vec<u8>>,
        mode: u32,
    },
    /// A link or special file.
    Unsafe,
}

fn read_entry(directory: &TrustedDir, name: &str, max_bytes: usize) -> Entry {
    let metadata = match fs::symlink_metadata(directory.path().join(name)) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Entry::Missing,
        Err(_error) => return Entry::Unsafe,
    };
    if !metadata.file_type().is_file() {
        return Entry::Unsafe;
    }
    let mode = metadata.mode() & MODE_BITS;
    let bytes = if mode & OTHERS_WRITE_BITS == 0 {
        directory.read_file(name, mode, max_bytes).ok()
    } else {
        None
    };
    Entry::File { bytes, mode }
}

fn hex_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn inspect_site(site: &Site) -> (Inspection, Option<Chain>) {
    let chain = match open_chain(site, false) {
        Ok(Some(chain)) => chain,
        Ok(None) | Err(OpenFault::RootMissing) => {
            return (Inspection::of(AgentSkillState::Absent), None)
        }
        Err(OpenFault::Unsafe(detail)) => {
            warn!(%detail, "the agent skill location is not safe");
            return (Inspection::of(AgentSkillState::Unsafe), None);
        }
    };
    let inspection = inspect_directory(&chain.skill);
    (inspection, Some(chain))
}

fn inspect_directory(directory: &TrustedDir) -> Inspection {
    let marker = read_entry(directory, MARKER_FILE, MAX_MARKER_BYTES);
    let skill = read_entry(directory, SKILL_FILE, MAX_SKILL_BYTES);
    if matches!(marker, Entry::Unsafe) || matches!(skill, Entry::Unsafe) {
        return Inspection::of(AgentSkillState::Unsafe);
    }
    let marker = match marker {
        Entry::File {
            bytes: Some(bytes), ..
        } => Marker::parse(&bytes),
        _ => None,
    };
    let (installed_sha256, skill_mode) = match &skill {
        Entry::File { bytes, mode } => (bytes.as_deref().map(hex_sha256), Some(*mode)),
        _ => (None, None),
    };
    let state = match (&marker, &skill) {
        (None, Entry::Missing) => match directory.entry_names_limited(MAX_ENTRIES) {
            Ok((names, _truncated)) if names.is_empty() => AgentSkillState::Absent,
            _ => AgentSkillState::Foreign,
        },
        (None, _) => AgentSkillState::Foreign,
        (Some(_), Entry::Missing) => AgentSkillState::Stale,
        (Some(marker), _) => match installed_sha256.as_deref() {
            Some(found) if found == marker.sha256 && found == agent_skill_sha256() => {
                AgentSkillState::Current
            }
            Some(found) if found == marker.sha256 => AgentSkillState::Stale,
            _ => AgentSkillState::Modified,
        },
    };
    Inspection {
        state,
        installed_sha256,
        skill_mode,
    }
}

/// The read-only skill state of `target`, and a warning when its location does
/// not resolve.
pub(super) fn status_of(target: &Target) -> (AgentSkillStatus, Option<String>) {
    let expected_sha256 = agent_skill_sha256();
    let Some(site) = site_of(target) else {
        return (
            AgentSkillStatus {
                state: AgentSkillState::Unsupported,
                path: None,
                installed_sha256: None,
                expected_sha256,
                shared: false,
            },
            None,
        );
    };
    match site {
        Err(failure) => (
            AgentSkillStatus {
                state: AgentSkillState::Absent,
                path: None,
                installed_sha256: None,
                expected_sha256,
                shared: false,
            },
            Some(format!(
                "the agent skill location does not resolve: {}",
                failure.msg
            )),
        ),
        Ok(site) => {
            let (inspection, _chain) = inspect_site(&site);
            (
                AgentSkillStatus {
                    state: inspection.state,
                    path: Some(site.skill_path().display().to_string()),
                    installed_sha256: inspection.installed_sha256,
                    expected_sha256,
                    shared: site.shared,
                },
                None,
            )
        }
    }
}

fn temporary_name() -> Result<String, ProtocolError> {
    let mut random = [0_u8; TEMPORARY_RANDOM_BYTES];
    getrandom::getrandom(&mut random)
        .map_err(|_error| unsafe_error("the system random source failed", None))?;
    let mut name = String::from(TEMPORARY_PREFIX);
    for byte in random {
        let _ = write!(name, "{byte:02x}");
    }
    Ok(name)
}

fn write_failure(path: &Path, error: &dyn std::fmt::Display) -> ProtocolError {
    warn!(%error, "the agent skill could not be written");
    unsafe_error("the skill could not be written", Some(path))
}

fn refused(found: AgentSkillState, path: &Path, failure: ProtocolError) -> AgentSkillReport {
    AgentSkillReport {
        state: AgentSkillChange::Refused,
        found,
        path: path.display().to_string(),
        error: Some(failure),
    }
}

/// Installs or refreshes the skill in the home of `target`.
///
/// `None` when the runtime declares no skill location; the caller refuses an
/// explicit request for such a runtime before any write.
pub(super) fn install_for(target: &Target) -> Option<AgentSkillReport> {
    let site = match site_of(target)? {
        Ok(site) => site,
        Err(failure) => return Some(refused(AgentSkillState::Absent, Path::new(""), failure)),
    };
    let path = site.skill_path();
    let runtime = target.resolved.runtime.as_wire().to_owned();
    let (inspection, _chain) = inspect_site(&site);
    let found = inspection.state;
    let report = |state| AgentSkillReport {
        state,
        found,
        path: path.display().to_string(),
        error: None,
    };
    Some(match found {
        AgentSkillState::Current => report(AgentSkillChange::Unchanged),
        AgentSkillState::Foreign => refused(
            found,
            &path,
            error(
                FOREIGN_CODE,
                format!(
                    "a skill named {SKILL_NAME} exists at {} and pohunek did not install it",
                    path.display()
                ),
                format!(
                    "move it away, then run `pohunek integration install --agent {runtime} --skill`"
                ),
            ),
        ),
        AgentSkillState::Modified => refused(
            found,
            &path,
            error(
                MODIFIED_CODE,
                format!(
                    "the installed skill at {} was edited since pohunek wrote it",
                    path.display()
                ),
                format!(
                    "run `pohunek integration uninstall --agent {runtime} --skill`, then install it again"
                ),
            ),
        ),
        AgentSkillState::Unsafe => refused(
            found,
            &path,
            unsafe_error("a link or an unsafe directory lies on the path", Some(&path)),
        ),
        AgentSkillState::Absent | AgentSkillState::Stale => {
            let change = if found == AgentSkillState::Absent {
                AgentSkillChange::Created
            } else {
                AgentSkillChange::Refreshed
            };
            match write_skill(&site, &inspection) {
                Ok(()) => report(change),
                Err(failure) => refused(found, &path, failure),
            }
        }
        _ => refused(
            found,
            &path,
            unsafe_error("the skill location is in an unknown state", Some(&path)),
        ),
    })
}

/// Writes the skill file, then its marker.
fn write_skill(site: &Site, inspection: &Inspection) -> Result<(), ProtocolError> {
    let path = site.skill_path();
    let chain = match open_chain(site, true) {
        Ok(Some(chain)) => chain,
        Ok(None) => return Err(unsafe_error("the skill directory is missing", Some(&path))),
        Err(OpenFault::RootMissing) => {
            return Err(unsafe_error(
                "the agent directory does not exist",
                Some(&path),
            ))
        }
        Err(OpenFault::Unsafe(detail)) => {
            warn!(%detail, "the agent skill location is not safe");
            return Err(unsafe_error(
                "a link or an unsafe directory lies on the path",
                Some(&path),
            ));
        }
    };
    let content = AGENT_SKILL.as_bytes();
    let marker = Marker::for_content(agent_skill_sha256()).render();
    let directory = &chain.skill;
    let write_marker = || match read_entry(directory, MARKER_FILE, MAX_MARKER_BYTES) {
        Entry::Missing => directory
            .create_file(MARKER_FILE, &marker, FILE_MODE)
            .map(|_identity| ())
            .map_err(|failure| write_failure(&path, &failure)),
        _ => directory
            .replace_file(MARKER_FILE, temporary_name()?, &marker, FILE_MODE)
            .map_err(|failure| write_failure(&path, &failure)),
    };
    if inspection.skill_mode.is_none() {
        // The marker goes first, so a write that stops between the two leaves
        // an owned skill whose file is missing (`stale`), never an unmarked
        // file that reads as foreign.
        write_marker()?;
        return directory
            .create_file(SKILL_FILE, content, FILE_MODE)
            .map(|_identity| ())
            .map_err(|failure| write_failure(&path, &failure));
    }
    let expected = inspection.installed_sha256.clone();
    let mode = inspection.skill_mode.unwrap_or(FILE_MODE);
    directory
        .replace_file_checked(SKILL_FILE, temporary_name()?, content, FILE_MODE, || {
            let bytes = directory.read_file(SKILL_FILE, mode, MAX_SKILL_BYTES)?;
            if Some(hex_sha256(&bytes)) == expected {
                Ok(())
            } else {
                Err(FsError::IdentityChanged {
                    path: directory.path().join(SKILL_FILE),
                })
            }
        })
        .map_err(|failure| write_failure(&path, &failure))?;
    write_marker()
}

/// Removes the owned skill from the home of `target`.
///
/// `None` when the runtime declares no skill location. A foreign or unsafe
/// skill is left alone; a skill in a location every home shares is kept when
/// the request removes one profile's integration only.
///
/// # Errors
///
/// `integration_agent_skill_unsafe` when an owned skill could not be removed.
pub(super) fn remove_for(
    target: &Target,
    selection: &HomeSelection,
) -> Result<Option<AgentSkillRemoval>, ProtocolError> {
    let Some(site) = site_of(target) else {
        return Ok(None);
    };
    let Ok(site) = site else {
        return Ok(Some(AgentSkillRemoval::NotInstalled));
    };
    let (inspection, chain) = inspect_site(&site);
    Ok(Some(match (inspection.state, chain) {
        (AgentSkillState::Foreign | AgentSkillState::Unsafe, _) => AgentSkillRemoval::LeftForeign,
        (AgentSkillState::Absent, _) | (_, None) => AgentSkillRemoval::NotInstalled,
        (_, Some(chain)) => {
            if site.shared && matches!(selection, HomeSelection::Profile(_)) {
                return Ok(Some(AgentSkillRemoval::KeptShared));
            }
            remove_owned(&chain, &site.skill_path())?
        }
    }))
}

/// Removes the skill file, its marker and the then-empty skill directory.
fn remove_owned(chain: &Chain, path: &Path) -> Result<AgentSkillRemoval, ProtocolError> {
    for name in [SKILL_FILE, MARKER_FILE] {
        chain.skill.remove_entry(name).map_err(|failure| {
            warn!(error = %failure, "an installed agent skill file was not removed");
            unsafe_error("the installed skill could not be removed", Some(path))
        })?;
    }
    if let Err(failure) = chain.parent.remove_empty_child(SKILL_NAME) {
        warn!(error = %failure, "the agent skill directory was not removed");
    }
    Ok(AgentSkillRemoval::Removed)
}
