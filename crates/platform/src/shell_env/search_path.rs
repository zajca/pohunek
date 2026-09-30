//! Validated executable search paths and the built-in fallback directories.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::filesystem::TrustedDir;

/// Longest accepted joined `PATH` value in bytes.
///
/// Equal to the supervisor's job value limit, because the value becomes an
/// environment entry of a native job definition that rejects longer values.
pub const MAX_SEARCH_PATH_BYTES: usize = crate::supervisor::MAX_JOB_VALUE_BYTES;

/// Separator between `PATH` entries.
const SEPARATOR: char = ':';

/// Directories searched when no configured or discovered `PATH` exists.
///
/// An entry starting with `~/` is relative to the user's home directory; every
/// other entry is absolute. The list is data, not logic, and is the only place
/// a platform directory is named. It never assumes one Homebrew prefix:
///
/// - `~/.local/bin`, `~/.cargo/bin`, `~/.bun/bin`: per-user installers (uv,
///   pipx, the native agent installers, `cargo install`, bun global installs).
/// - `/opt/homebrew/{bin,sbin}`: the Apple Silicon Homebrew prefix.
/// - `/usr/local/{bin,sbin}`: the Intel Homebrew prefix and vendor installers.
/// - `/usr/bin`, `/bin`, `/usr/sbin`, `/sbin`: the system tools launchd itself
///   provides, kept last so user installs may shadow them like in a shell.
///
/// Only directories that exist are used, so an absent prefix costs nothing and
/// a prefix installed later is picked up by the next `pohunek service install`.
pub const DARWIN_FALLBACK_DIRECTORIES: &[&str] = &[
    "~/.local/bin",
    "~/.cargo/bin",
    "~/.bun/bin",
    "/opt/homebrew/bin",
    "/opt/homebrew/sbin",
    "/usr/local/bin",
    "/usr/local/sbin",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
];

/// Prefix marking a fallback entry as relative to the home directory.
const HOME_PREFIX: &str = "~/";

/// Reports an unusable search path or entry.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum SearchPathError {
    /// An entry is not an absolute, normalized, UTF-8 directory path.
    #[error("search path entry is invalid: {reason}")]
    InvalidEntry {
        /// Why the entry was rejected; never contains the entry itself.
        reason: &'static str,
    },
    /// The same directory appears twice.
    #[error("search path lists a directory twice")]
    Duplicate,
    /// The joined value exceeds [`MAX_SEARCH_PATH_BYTES`].
    #[error("search path is {actual} bytes; maximum is {maximum}")]
    TooLong {
        /// Observed joined length.
        actual: usize,
        /// Maximum accepted joined length.
        maximum: usize,
    },
    /// The value holds a NUL or other control character.
    #[error("search path contains a control character")]
    ControlCharacter,
    /// Nothing usable remained after validation.
    #[error("search path holds no usable directory")]
    NoUsableDirectories,
}

/// An ordered list of validated executable search directories.
///
/// Every entry is an absolute, normalized, UTF-8 path without `:` or control
/// characters, and no directory repeats. The joined form fits
/// [`MAX_SEARCH_PATH_BYTES`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchPath {
    entries: Vec<PathBuf>,
}

/// One directory refused because it is not safe to search for programs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedEntry {
    /// The entry as it was listed.
    pub entry: String,
    /// Why it was refused.
    pub reason: &'static str,
}

/// One directory recorded as its canonical path instead of as listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalizedEntry {
    /// The entry as it was listed.
    pub entry: String,
    /// The canonical path recorded instead.
    pub recorded: String,
}

/// Outcome of sanitizing an untrusted `PATH` value or a fallback table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizedPath {
    /// The usable directories in their original order. An entry is recorded as
    /// listed when its whole symlink chain is owner-controlled, and as its
    /// canonical path otherwise (see [`SanitizedPath::canonicalized`]).
    pub path: SearchPath,
    /// Entries recorded as their canonical path because a symlink on the way
    /// could be retargeted by another account; callers report these.
    pub canonicalized: Vec<CanonicalizedEntry>,
    /// Existing directories refused as untrusted; callers report these.
    pub untrusted: Vec<DroppedEntry>,
    /// Entries ignored without concern: empty, relative, `.`-style, duplicate,
    /// or missing (a stale `PATH` entry is ordinary).
    pub ignored: usize,
}

/// Why a directory failed the trust check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustError {
    /// The entry does not exist or is not a directory.
    Missing,
    /// The entry exists but is unsafe; the payload says why.
    Refused(&'static str),
}

/// Checks that `entry` is safe to search for programs.
///
/// Another local account able to write into a searched directory could plant
/// an agent executable that later runs as the owner, so the rules are strict:
/// symlinks are resolved first; every component of the canonical path must
/// pass the platform's trusted-ancestor policy (effective-user-owned
/// components are not group- or world-writable, root-owned ones are
/// non-writable or sticky, no foreign owners); and the final directory itself
/// must not be writable by group or others, which also refuses sticky
/// world-writable directories such as `/tmp` and group-writable setups such as
/// an admin-group `/usr/local/bin` (out of scope).
///
/// Returns the canonical path the checks ran against. A caller may record the
/// entry as listed only when [`owner_controlled_chain`] holds, so a dotfile or
/// profile symlink keeps following its target; otherwise it records this path.
///
/// # Errors
///
/// Returns [`TrustError::Missing`] for an absent or non-directory entry and
/// [`TrustError::Refused`] for an unsafe one.
pub fn trusted_directory(entry: &Path) -> Result<PathBuf, TrustError> {
    use std::os::unix::fs::MetadataExt as _;

    /// Group and other write permission bits.
    const GROUP_OTHER_WRITE: u32 = 0o022;

    let canonical = std::fs::canonicalize(entry).map_err(|_absent| TrustError::Missing)?;
    let metadata = std::fs::metadata(&canonical).map_err(|_absent| TrustError::Missing)?;
    if !metadata.is_dir() {
        return Err(TrustError::Missing);
    }
    if metadata.mode() & GROUP_OTHER_WRITE != 0 {
        return Err(TrustError::Refused("writable by group or others"));
    }
    TrustedDir::open_absolute_ancestor(&canonical).map_err(|_unsafe| {
        TrustError::Refused("a path component is foreign-owned or writable by others")
    })?;
    Ok(canonical)
}

/// Whether every symlink on the way to `entry`, targets included, is
/// controlled by the effective user or root.
///
/// The path is resolved component by component. Each symlink must be owned by
/// the effective user or root, and the canonical directory that holds it must be
/// owned by one of them and not writable by group or others; a sticky
/// world-writable directory such as `/tmp` does not qualify. Only then can no
/// other account retarget a link and redirect a recorded entry.
#[must_use]
pub fn owner_controlled_chain(entry: &Path) -> bool {
    use std::ffi::OsString;
    use std::os::unix::fs::MetadataExt as _;

    /// Symlinks followed before the path is treated as a loop.
    const MAX_LINKS: usize = 40;
    /// Group and other write permission bits.
    const GROUP_OTHER_WRITE: u32 = 0o022;

    let Ok(root) = std::fs::metadata("/") else {
        return false;
    };
    let root_uid = root.uid();
    let effective_uid = rustix::process::geteuid().as_raw();
    let controlled = |uid: u32| uid == effective_uid || uid == root_uid;

    let mut pending: std::collections::VecDeque<OsString> = entry
        .components()
        .filter_map(|component| match component {
            std::path::Component::RootDir | std::path::Component::CurDir => None,
            other => Some(other.as_os_str().to_owned()),
        })
        .collect();
    let mut resolved = PathBuf::from("/");
    let mut links = 0_usize;
    while let Some(name) = pending.pop_front() {
        if name == ".." {
            resolved.pop();
            continue;
        }
        let next = resolved.join(&name);
        let Ok(metadata) = std::fs::symlink_metadata(&next) else {
            return false;
        };
        if !metadata.file_type().is_symlink() {
            resolved = next;
            continue;
        }
        links += 1;
        if links > MAX_LINKS || !controlled(metadata.uid()) {
            return false;
        }
        let Ok(holder) = std::fs::metadata(&resolved) else {
            return false;
        };
        if !controlled(holder.uid()) || holder.mode() & GROUP_OTHER_WRITE != 0 {
            return false;
        }
        let Ok(target) = std::fs::read_link(&next) else {
            return false;
        };
        if target.is_absolute() {
            resolved = PathBuf::from("/");
        }
        for component in target.components().rev() {
            match component {
                std::path::Component::RootDir | std::path::Component::CurDir => {}
                other => pending.push_front(other.as_os_str().to_owned()),
            }
        }
    }
    true
}

/// Validates one search directory entry.
///
/// # Errors
///
/// Returns [`SearchPathError::InvalidEntry`] for an empty, relative, non-UTF-8,
/// `:`-containing, or non-normalized (`.`/`..` segment) path and
/// [`SearchPathError::ControlCharacter`] for a control character.
pub fn validate_search_directory(entry: &Path) -> Result<(), SearchPathError> {
    let Some(text) = entry.to_str() else {
        return Err(SearchPathError::InvalidEntry {
            reason: "not valid UTF-8",
        });
    };
    if text.chars().any(char::is_control) {
        return Err(SearchPathError::ControlCharacter);
    }
    if text.is_empty() {
        return Err(SearchPathError::InvalidEntry { reason: "empty" });
    }
    if text.contains(SEPARATOR) {
        return Err(SearchPathError::InvalidEntry {
            reason: "contains the `:` separator",
        });
    }
    if !entry.is_absolute() {
        return Err(SearchPathError::InvalidEntry {
            reason: "not absolute",
        });
    }
    if text
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return Err(SearchPathError::InvalidEntry {
            reason: "contains a `.` or `..` segment",
        });
    }
    Ok(())
}

impl SearchPath {
    /// Returns a search path without directories.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Validates `entries` exactly as given.
    ///
    /// Nothing is dropped or reordered, so the value round-trips through a
    /// configuration file unchanged. Existence is not checked.
    ///
    /// # Errors
    ///
    /// Returns the [`validate_search_directory`] errors,
    /// [`SearchPathError::Duplicate`], and [`SearchPathError::TooLong`].
    pub fn new(entries: Vec<PathBuf>) -> Result<Self, SearchPathError> {
        for (index, entry) in entries.iter().enumerate() {
            validate_search_directory(entry)?;
            if entries[..index].contains(entry) {
                return Err(SearchPathError::Duplicate);
            }
        }
        let path = Self { entries };
        let length = path.to_env_value().len();
        if length > MAX_SEARCH_PATH_BYTES {
            return Err(SearchPathError::TooLong {
                actual: length,
                maximum: MAX_SEARCH_PATH_BYTES,
            });
        }
        Ok(path)
    }

    /// Sanitizes an untrusted `PATH` value such as a login shell printed.
    ///
    /// Entries that are empty, relative, `.`-style, or duplicated are ignored
    /// and counted. With `trusted_only`, an entry must also be a trusted
    /// directory, see [`trusted_directory`]: missing ones are ignored, unsafe
    /// ones are returned in [`SanitizedPath::untrusted`] with a reason. Kept
    /// entries are recorded as listed; duplicates are detected on the
    /// canonical path.
    ///
    /// # Errors
    ///
    /// Returns [`SearchPathError::ControlCharacter`] when the value holds a
    /// control character at all (the value is then garbage, not a `PATH`),
    /// [`SearchPathError::NoUsableDirectories`] when nothing survives, and
    /// [`SearchPathError::TooLong`] when the survivors exceed the bound.
    pub fn sanitize(value: &str, trusted_only: bool) -> Result<SanitizedPath, SearchPathError> {
        if value.chars().any(char::is_control) {
            return Err(SearchPathError::ControlCharacter);
        }
        let mut kept = Kept::default();
        for raw in value.split(SEPARATOR) {
            kept.consider(PathBuf::from(raw), trusted_only);
        }
        kept.finish()
    }

    /// Returns this path followed by the `extra` directories it lacks.
    ///
    /// Directories are appended one by one while the joined value fits
    /// [`MAX_SEARCH_PATH_BYTES`]; the rest are left out.
    #[must_use]
    pub fn with_appended(&self, extra: &[PathBuf]) -> Self {
        let mut entries = self.entries.clone();
        for entry in extra {
            if entries.contains(entry) {
                continue;
            }
            entries.push(entry.clone());
            let candidate = Self {
                entries: entries.clone(),
            };
            if candidate.to_env_value().len() > MAX_SEARCH_PATH_BYTES {
                entries.pop();
            }
        }
        Self { entries }
    }

    /// Returns the directories in search order.
    #[must_use]
    pub fn entries(&self) -> &[PathBuf] {
        &self.entries
    }

    /// Returns whether no directory is listed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the `:`-joined `PATH` value.
    #[must_use]
    pub fn to_env_value(&self) -> String {
        let mut value = String::new();
        for (index, entry) in self.entries.iter().enumerate() {
            if index > 0 {
                value.push(SEPARATOR);
            }
            // Entries are validated UTF-8 at construction.
            value.push_str(&entry.to_string_lossy());
        }
        value
    }
}

/// Collects the kept entries of one sanitization pass.
#[derive(Default)]
struct Kept {
    entries: Vec<PathBuf>,
    canonical: Vec<PathBuf>,
    canonicalized: Vec<CanonicalizedEntry>,
    untrusted: Vec<DroppedEntry>,
    ignored: usize,
}

impl Kept {
    fn consider(&mut self, entry: PathBuf, trusted_only: bool) {
        if validate_search_directory(&entry).is_err() {
            self.ignored += 1;
            return;
        }
        let mut entry = entry;
        let identity = if trusted_only {
            match trusted_directory(&entry) {
                Ok(canonical) => {
                    if !owner_controlled_chain(&entry) && canonical != entry {
                        self.canonicalized.push(CanonicalizedEntry {
                            entry: entry.to_string_lossy().into_owned(),
                            recorded: canonical.to_string_lossy().into_owned(),
                        });
                        entry.clone_from(&canonical);
                    }
                    canonical
                }
                Err(TrustError::Missing) => {
                    self.ignored += 1;
                    return;
                }
                Err(TrustError::Refused(reason)) => {
                    self.untrusted.push(DroppedEntry {
                        entry: entry.to_string_lossy().into_owned(),
                        reason,
                    });
                    return;
                }
            }
        } else {
            entry.clone()
        };
        if self.canonical.contains(&identity) {
            self.ignored += 1;
        } else {
            self.canonical.push(identity);
            self.entries.push(entry);
        }
    }

    fn finish(self) -> Result<SanitizedPath, SearchPathError> {
        if self.entries.is_empty() {
            return Err(SearchPathError::NoUsableDirectories);
        }
        Ok(SanitizedPath {
            path: SearchPath::new(self.entries)?,
            canonicalized: self.canonicalized,
            untrusted: self.untrusted,
            ignored: self.ignored,
        })
    }
}

/// Builds the search path of the trusted directories in `table`.
///
/// `~/` entries expand against `home`; without a home they are skipped.
/// Missing directories are ignored, unsafe ones (see [`trusted_directory`]) are
/// returned in [`SanitizedPath::untrusted`], and kept entries are recorded as
/// listed.
///
/// # Errors
///
/// Returns [`SearchPathError::NoUsableDirectories`] when no entry is usable.
pub fn fallback_search_path(
    table: &[&str],
    home: Option<&Path>,
) -> Result<SanitizedPath, SearchPathError> {
    let mut kept = Kept::default();
    for item in table {
        let entry = match item.strip_prefix(HOME_PREFIX) {
            Some(relative) => match home {
                Some(home) => home.join(relative),
                None => continue,
            },
            None => PathBuf::from(item),
        };
        kept.consider(entry, true);
    }
    kept.finish()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::shell_env::test_support::{fixture, make_dir};

    #[test]
    fn entries_must_be_absolute_normalized_and_free_of_separators() {
        for (entry, ok) in [
            ("/usr/bin", true),
            ("/opt/my tools/b\u{e9}n", true),
            ("/it's/\"q\"", true),
            ("", false),
            (".", false),
            ("bin", false),
            ("/usr/./bin", false),
            ("/usr/../bin", false),
            ("/a:b", false),
            ("/a\nb", false),
            ("/a\0b", false),
            ("/a\tb", false),
        ] {
            assert_eq!(
                validate_search_directory(Path::new(entry)).is_ok(),
                ok,
                "{entry:?}"
            );
        }
    }

    #[test]
    fn new_rejects_duplicates_and_oversized_values_and_round_trips() {
        assert_eq!(
            SearchPath::new(vec!["/a".into(), "/a".into()]),
            Err(SearchPathError::Duplicate)
        );
        let long = format!("/{}", "x".repeat(MAX_SEARCH_PATH_BYTES));
        assert!(matches!(
            SearchPath::new(vec![long.into()]),
            Err(SearchPathError::TooLong { .. })
        ));
        let path = SearchPath::new(vec!["/b".into(), "/a".into()]).expect("path");
        assert_eq!(path.to_env_value(), "/b:/a");
        assert!(SearchPath::empty().is_empty());
    }

    #[test]
    fn sanitize_ignores_bad_entries_and_records_the_lexical_path() {
        let dir = fixture();
        let bin = dir.path().join("b in");
        make_dir(dir.path(), &bin);
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&bin, &link).expect("symlink");
        let value = format!(
            "{}::.:rel:{}:{}:{}",
            bin.display(),
            bin.display(),
            link.display(),
            dir.path().join("gone").display()
        );
        let sanitized = SearchPath::sanitize(&value, true).expect("sanitize");
        // The symlink resolves to the same directory and is a duplicate; the
        // entry is recorded as listed, not as its canonical path.
        assert_eq!(sanitized.path.entries(), [bin]);
        assert_eq!(sanitized.ignored, 6);
        assert!(sanitized.untrusted.is_empty());
        let unchecked = SearchPath::sanitize("/no/such/dir", false).expect("unchecked");
        assert_eq!(unchecked.path.to_env_value(), "/no/such/dir");
    }

    #[test]
    fn a_symlinked_entry_is_recorded_as_listed() {
        let dir = fixture();
        let target = dir.path().join("profile-1/bin");
        make_dir(dir.path(), &target);
        let link = dir.path().join("current");
        std::os::unix::fs::symlink(dir.path().join("profile-1"), &link).expect("symlink");
        let entry = link.join("bin");
        let sanitized = SearchPath::sanitize(&entry.display().to_string(), true).expect("ok");
        assert_eq!(sanitized.path.entries(), [entry]);
    }

    #[test]
    fn untrusted_directories_are_refused_and_reported_with_a_reason() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = fixture();
        let root = dir.path();
        let good = root.join("opt/homebrew/bin");
        make_dir(root, &good);
        let mode = |path: &Path, bits: u32| {
            fs::set_permissions(path, fs::Permissions::from_mode(bits)).expect("chmod");
        };
        let group_writable = root.join("group-writable");
        make_dir(root, &group_writable);
        mode(&group_writable, 0o775);
        let world_writable = root.join("world-writable");
        // An entry below a writable ancestor is refused although it is 0755.
        let below = world_writable.join("inner");
        make_dir(root, &below);
        mode(&world_writable, 0o777);
        let sticky = root.join("sticky");
        make_dir(root, &sticky);
        mode(&sticky, 0o1777);
        // A symlink to a writable directory resolves to it and is refused.
        let link = root.join("link-into-writable");
        std::os::unix::fs::symlink(&world_writable, &link).expect("symlink");
        let value = [
            &good,
            &group_writable,
            &world_writable,
            &sticky,
            &link,
            &below,
        ]
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(":");
        let sanitized = SearchPath::sanitize(&value, true).expect("sanitize");
        assert_eq!(sanitized.path.entries(), [good]);
        let refused: Vec<&str> = sanitized
            .untrusted
            .iter()
            .map(|dropped| dropped.entry.as_str())
            .collect();
        assert_eq!(refused.len(), 5, "{refused:?}");
        assert!(sanitized
            .untrusted
            .iter()
            .all(|dropped| !dropped.reason.is_empty()));
        assert_eq!(
            trusted_directory(Path::new("/tmp")),
            Err(TrustError::Refused("writable by group or others"))
        );
        let only_untrusted = SearchPath::sanitize(&group_writable.display().to_string(), true);
        assert_eq!(only_untrusted, Err(SearchPathError::NoUsableDirectories));
    }

    /// A world-writable directory (not sticky) standing in for `/tmp`.
    fn wild_dir(root: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let wild = root.join(name);
        make_dir(root, &wild);
        fs::set_permissions(&wild, fs::Permissions::from_mode(0o777)).expect("chmod");
        wild
    }

    #[test]
    fn a_symlink_in_an_untrusted_directory_is_recorded_as_its_canonical_path() {
        let dir = fixture();
        let root = dir.path();
        let good = root.join("good/bin");
        make_dir(root, &good);
        let wild = wild_dir(root, "wild");
        let link = wild.join("tools");
        std::os::unix::fs::symlink(&good, &link).expect("symlink");
        assert!(!owner_controlled_chain(&link));
        let sanitized = SearchPath::sanitize(&link.display().to_string(), true).expect("ok");
        assert_eq!(sanitized.path.entries(), std::slice::from_ref(&good));
        assert_eq!(
            sanitized.canonicalized,
            [CanonicalizedEntry {
                entry: link.display().to_string(),
                recorded: good.display().to_string(),
            }]
        );
        // Retargeting the link afterwards cannot change the recorded entry.
        let evil = root.join("evil/bin");
        make_dir(root, &evil);
        fs::remove_file(&link).expect("unlink");
        std::os::unix::fs::symlink(&evil, &link).expect("retarget");
        assert_eq!(sanitized.path.entries(), [good]);
    }

    #[test]
    fn a_symlink_in_an_owner_only_directory_is_recorded_as_listed() {
        let dir = fixture();
        let root = dir.path();
        let good = root.join("profile-1/bin");
        make_dir(root, &good);
        let home = root.join("home");
        make_dir(root, &home);
        let link = home.join("current");
        std::os::unix::fs::symlink(root.join("profile-1"), &link).expect("symlink");
        let entry = link.join("bin");
        assert!(owner_controlled_chain(&entry));
        let sanitized = SearchPath::sanitize(&entry.display().to_string(), true).expect("ok");
        assert_eq!(sanitized.path.entries(), [entry]);
        assert!(sanitized.canonicalized.is_empty());
    }

    #[test]
    fn an_untrusted_symlink_behind_a_trusted_one_is_found_through_its_target() {
        let dir = fixture();
        let root = dir.path();
        let good = root.join("good/bin");
        make_dir(root, &good);
        let wild = wild_dir(root, "wild");
        let inner = wild.join("inner");
        std::os::unix::fs::symlink(&good, &inner).expect("inner link");
        // Owner-only directory, but its link points at a link others control.
        let home = root.join("home");
        make_dir(root, &home);
        let outer = home.join("outer");
        std::os::unix::fs::symlink(&inner, &outer).expect("outer link");
        assert!(!owner_controlled_chain(&outer));
        let sanitized = SearchPath::sanitize(&outer.display().to_string(), true).expect("ok");
        assert_eq!(sanitized.path.entries(), [good]);
        assert_eq!(sanitized.canonicalized.len(), 1);
    }

    #[test]
    fn a_symlink_loop_is_not_owner_controlled() {
        let dir = fixture();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::os::unix::fs::symlink(&b, &a).expect("a");
        std::os::unix::fs::symlink(&a, &b).expect("b");
        assert!(!owner_controlled_chain(&a));
    }

    #[test]
    fn a_mode_0755_directory_of_ours_is_trusted() {
        let dir = fixture();
        let bin = dir.path().join("bin");
        make_dir(dir.path(), &bin);
        assert_eq!(trusted_directory(&bin), Ok(bin));
        assert_eq!(
            trusted_directory(&dir.path().join("missing")),
            Err(TrustError::Missing)
        );
        let file = dir.path().join("file");
        fs::write(&file, "x").expect("file");
        assert_eq!(trusted_directory(&file), Err(TrustError::Missing));
    }

    #[test]
    fn appending_skips_present_entries_and_respects_the_bound() {
        let base = SearchPath::new(vec!["/a".into(), "/b".into()]).expect("base");
        let merged = base.with_appended(&["/b".into(), "/c".into()]);
        assert_eq!(merged.to_env_value(), "/a:/b:/c");
        let long = PathBuf::from(format!("/{}", "x".repeat(MAX_SEARCH_PATH_BYTES - 4)));
        let bounded = base.with_appended(&[long, "/d".into()]);
        assert_eq!(bounded.to_env_value(), "/a:/b:/d");
    }

    #[test]
    fn sanitize_fails_closed_on_control_characters_and_empty_results() {
        assert_eq!(
            SearchPath::sanitize("/usr/bin\u{1b}[0m:/bin", false),
            Err(SearchPathError::ControlCharacter)
        );
        assert_eq!(
            SearchPath::sanitize("::.", false),
            Err(SearchPathError::NoUsableDirectories)
        );
    }

    #[test]
    fn the_builtin_fallback_table_is_valid_and_prefix_agnostic() {
        assert!(DARWIN_FALLBACK_DIRECTORIES.contains(&"/opt/homebrew/bin"));
        assert!(DARWIN_FALLBACK_DIRECTORIES.contains(&"/usr/local/bin"));
        let home = Path::new("/Users/me");
        for entry in DARWIN_FALLBACK_DIRECTORIES {
            let expanded = entry
                .strip_prefix(HOME_PREFIX)
                .map_or_else(|| PathBuf::from(entry), |relative| home.join(relative));
            assert!(validate_search_directory(&expanded).is_ok(), "{entry}");
        }
    }
}
