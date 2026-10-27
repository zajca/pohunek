//! The directory-name existence check of a hook runtime.
//!
//! Some agents keep a conversation as a directory named after its id, with a
//! fixed file inside, below a date-sharded tree:
//! `<root>/<dir>/YYYY/MM/DD/<reference>/session.jsonl`. A runtime declares the
//! check next to a `hook` strategy:
//!
//! ```toml
//! [native_reference.existence]
//! check = "file"
//! root_env = "XDG_DATA_HOME"
//! root_home = ".local/share"
//! dir = "muse/sessions"
//! dir_name = "{reference}"
//! file_name = "session.jsonl"
//! max_depth = 3
//! ```
//!
//! `max_depth` is the number of levels the scan descends below `dir`: the
//! directory named after the reference is an entry of the directory reached
//! after that many levels (`dir/YYYY/MM/DD` for 3), and the entries of
//! shallower levels are only descended into, newest name first.
//!
//! The declaration is descriptor-only. It is not an [`ExistenceSpec`], which is
//! shared with the persisted assigned-reference snapshot, so the stored shape
//! of an assigned reference does not change.

use std::ffi::{OsStr, OsString};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

use rustix::fs;
use serde::Deserialize;

use super::{
    canonicalize, classify_open_error, is_plain_component, open_child_directory, open_root,
    resolve_declared_root, validate_depth, validate_env_name, validate_relative_path,
    ExistenceCheckKind, ExistenceError, ExistenceSpec, NameMatch, ReferenceCheckFailure,
    ReferenceExistence, RelocatingSetting, MAX_CHECKED_REFERENCE_BYTES, MAX_EXISTENCE_ENTRIES,
    MAX_EXISTENCE_TEXT_BYTES, NAME_PLACEHOLDER,
};

/// A `[native_reference.existence]` table as a descriptor writes it: the
/// fields of an [`ExistenceSpec`] plus `dir_name`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExistenceDecl {
    /// Which check to run.
    pub check: ExistenceCheckKind,
    /// Environment variable that names the root.
    #[serde(default)]
    pub root_env: Option<String>,
    /// Root relative to the user's home directory while `root_env` is unset.
    #[serde(default)]
    pub root_home: Option<String>,
    /// Directory below the root that holds the conversations.
    #[serde(default)]
    pub dir: Option<String>,
    /// Name of the directory that holds the conversation file: `{reference}`.
    #[serde(default)]
    pub dir_name: Option<String>,
    /// File name template, or with `dir_name` the plain file name inside it.
    #[serde(default)]
    pub file_name: Option<String>,
    /// How `file_name` is compared with a directory entry.
    #[serde(default)]
    pub name_match: Option<NameMatch>,
    /// Directory levels below `dir` the scan descends.
    #[serde(default)]
    pub max_depth: Option<u8>,
    /// Environment variable that names the conversation directory itself.
    #[serde(default)]
    pub store_env: Option<String>,
    /// Directory levels below the `store_env` directory the scan descends.
    #[serde(default)]
    pub store_max_depth: Option<u8>,
    /// Launch arguments that move the store.
    #[serde(default)]
    pub relocating_args: Option<Vec<String>>,
    /// Settings-file keys that move the store.
    #[serde(default)]
    pub relocating_settings: Option<Vec<RelocatingSetting>>,
}

impl ExistenceDecl {
    /// The check of a hook runtime, where `dir_name` is accepted.
    ///
    /// # Errors
    ///
    /// Returns an [`ExistenceError`] naming the offending field.
    pub fn into_hook(self) -> Result<ReferenceExistence, ExistenceError> {
        if self.dir_name.is_some() {
            return DirNameExistence::new(self).map(ReferenceExistence::DirName);
        }
        self.into_persistable()
    }

    /// The check of an assigned reference, which is persisted and therefore
    /// cannot name a directory.
    ///
    /// # Errors
    ///
    /// Returns an [`ExistenceError`] naming the offending field.
    pub fn into_assigned(self) -> Result<ReferenceExistence, ExistenceError> {
        if self.dir_name.is_some() {
            return Err(ExistenceError::new(
                "dir_name",
                "is accepted only with strategy = \"hook\"",
            ));
        }
        self.into_persistable()
    }

    fn into_persistable(self) -> Result<ReferenceExistence, ExistenceError> {
        ReferenceExistence::try_from(ExistenceSpec {
            check: self.check,
            root_env: self.root_env,
            root_home: self.root_home,
            dir: self.dir,
            file_name: self.file_name,
            name_match: self.name_match,
            max_depth: self.max_depth,
            store_env: self.store_env,
            store_max_depth: self.store_max_depth,
            relocating_args: self.relocating_args,
            relocating_settings: self.relocating_settings,
        })
    }
}

/// A validated directory-name existence declaration. It never serializes: only
/// a hook runtime declares it, and its definition is not persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirNameExistence {
    root_env: Option<String>,
    root_home: Option<String>,
    dir: Option<String>,
    file_name: String,
    depth: u8,
}

impl DirNameExistence {
    fn new(decl: ExistenceDecl) -> Result<Self, ExistenceError> {
        if decl.check != ExistenceCheckKind::File {
            return Err(ExistenceError::new(
                "check",
                "must be \"file\" with dir_name",
            ));
        }
        if decl.dir_name.as_deref() != Some(NAME_PLACEHOLDER) {
            return Err(ExistenceError::new("dir_name", "must be {reference}"));
        }
        let rejected = [
            ("name_match", decl.name_match.is_some()),
            ("store_env", decl.store_env.is_some()),
            ("store_max_depth", decl.store_max_depth.is_some()),
            ("relocating_args", decl.relocating_args.is_some()),
            ("relocating_settings", decl.relocating_settings.is_some()),
        ]
        .into_iter()
        .find_map(|(field, present)| present.then_some(field));
        if let Some(field) = rejected {
            return Err(ExistenceError::new(field, "is not accepted with dir_name"));
        }
        if decl.root_env.is_none() && decl.root_home.is_none() {
            return Err(ExistenceError::new(
                "root_env",
                "root_env or root_home is required",
            ));
        }
        if let Some(name) = &decl.root_env {
            validate_env_name(name)?;
        }
        if let Some(path) = &decl.root_home {
            validate_relative_path(path, "root_home")?;
        }
        if let Some(path) = &decl.dir {
            validate_relative_path(path, "dir")?;
        }
        let file_name = decl
            .file_name
            .ok_or(ExistenceError::new("file_name", "is required"))?;
        if file_name.len() > MAX_EXISTENCE_TEXT_BYTES
            || file_name.contains(NAME_PLACEHOLDER)
            || !is_plain_component(&file_name)
        {
            return Err(ExistenceError::new(
                "file_name",
                "must be a plain file name without {reference} with dir_name",
            ));
        }
        let depth = decl
            .max_depth
            .ok_or(ExistenceError::new("max_depth", "is required"))?;
        validate_depth(depth, "max_depth")?;
        Ok(Self {
            root_env: decl.root_env,
            root_home: decl.root_home,
            dir: decl.dir,
            file_name,
            depth,
        })
    }

    /// Confirms `<dir>/…/<reference>/<file_name>` is a regular file and returns
    /// its status, found through directory descriptors that never follow a
    /// symlink.
    pub(super) fn stat(
        &self,
        reference: &str,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Result<fs::Stat, ReferenceCheckFailure> {
        if reference.len() > MAX_CHECKED_REFERENCE_BYTES || !is_plain_component(reference) {
            return Err(ReferenceCheckFailure::InvalidReference);
        }
        let root = canonicalize(&resolve_declared_root(
            self.root_env.as_deref(),
            self.root_home.as_deref(),
            env,
        )?)?;
        let mut directory = open_root(&root)?;
        for component in self.dir.iter().flat_map(|dir| dir.split('/')) {
            directory =
                open_child_directory(&directory, OsStr::new(component)).map_err(|errno| {
                    classify_open_error(errno, ReferenceCheckFailure::RootUnavailable)
                })?;
        }
        let mut visited = 0_usize;
        self.find(&directory, 0, reference, &mut visited)
    }

    /// Searches `directory`, `depth` levels below `dir`, newest name first.
    fn find(
        &self,
        directory: &OwnedFd,
        depth: u8,
        reference: &str,
        visited: &mut usize,
    ) -> Result<fs::Stat, ReferenceCheckFailure> {
        let entries =
            fs::Dir::read_from(directory).map_err(|_errno| ReferenceCheckFailure::Missing)?;
        let mut subdirectories: Vec<OsString> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|_errno| ReferenceCheckFailure::Missing)?;
            let name_bytes = entry.file_name().to_bytes();
            if name_bytes == b"." || name_bytes == b".." {
                continue;
            }
            *visited += 1;
            if *visited > MAX_EXISTENCE_ENTRIES {
                return Err(ReferenceCheckFailure::ScanLimit);
            }
            let name = OsStr::from_bytes(name_bytes);
            let Ok(stat) = fs::statat(directory, name, fs::AtFlags::SYMLINK_NOFOLLOW) else {
                continue;
            };
            if fs::FileType::from_raw_mode(stat.st_mode) != fs::FileType::Directory {
                continue;
            }
            if depth == self.depth {
                if name_bytes == reference.as_bytes() {
                    match self.open_file(directory, name) {
                        Err(ReferenceCheckFailure::Missing) => {}
                        outcome => return outcome,
                    }
                }
            } else {
                subdirectories.push(OsString::from_vec(name_bytes.to_vec()));
            }
        }
        subdirectories.sort_unstable_by(|a, b| b.cmp(a));
        for name in subdirectories {
            let child = match open_child_directory(directory, &name) {
                Ok(child) => child,
                Err(rustix::io::Errno::NOENT) => continue,
                Err(_errno) => return Err(ReferenceCheckFailure::StoreChanged),
            };
            match self.find(&child, depth + 1, reference, visited) {
                Err(ReferenceCheckFailure::Missing) => {}
                outcome => return outcome,
            }
        }
        Err(ReferenceCheckFailure::Missing)
    }

    /// Opens `<name>/<file_name>` below `directory` without following a
    /// symlink and returns its status when it is a regular file.
    fn open_file(
        &self,
        directory: &OwnedFd,
        name: &OsStr,
    ) -> Result<fs::Stat, ReferenceCheckFailure> {
        let conversation = open_child_directory(directory, name)
            .map_err(|errno| classify_open_error(errno, ReferenceCheckFailure::StoreChanged))?;
        let file = fs::openat(
            &conversation,
            OsStr::new(&self.file_name),
            fs::OFlags::RDONLY | fs::OFlags::NONBLOCK | fs::OFlags::NOFOLLOW | fs::OFlags::CLOEXEC,
            fs::Mode::empty(),
        )
        .map_err(|errno| classify_open_error(errno, ReferenceCheckFailure::StoreChanged))?;
        let stat = fs::fstat(&file).map_err(|_errno| ReferenceCheckFailure::StoreChanged)?;
        if fs::FileType::from_raw_mode(stat.st_mode) != fs::FileType::RegularFile {
            return Err(ReferenceCheckFailure::StoreChanged);
        }
        Ok(stat)
    }
}
