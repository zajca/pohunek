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

/// Outcome of sanitizing an untrusted `PATH` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizedPath {
    /// The usable directories in their original order.
    pub path: SearchPath,
    /// Entries dropped as empty, relative, `.`-style, duplicate, or missing.
    pub dropped: usize,
}

/// Returns the canonical path of `entry` when it is safe to search for programs.
///
/// Another local account able to write into a searched directory could plant
/// an agent executable that later runs as the owner, so the rules are strict:
/// symlinks are resolved first; every component of the canonical path must
/// pass the platform's trusted-ancestor policy (effective-user-owned
/// components are not group- or world-writable, root-owned ones are
/// non-writable or sticky, no foreign owners); and the final directory itself
/// must not be writable by group or others, which also refuses sticky
/// world-writable directories such as `/tmp`. A missing or non-directory
/// entry is refused too.
#[must_use]
pub fn trusted_directory(entry: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt as _;

    /// Group and other write permission bits.
    const GROUP_OTHER_WRITE: u32 = 0o022;

    let canonical = std::fs::canonicalize(entry).ok()?;
    TrustedDir::open_absolute_ancestor(&canonical).ok()?;
    let metadata = std::fs::metadata(&canonical).ok()?;
    (metadata.is_dir() && metadata.mode() & GROUP_OTHER_WRITE == 0).then_some(canonical)
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
    /// Entries that are empty, relative, `.`-style, or duplicated are dropped
    /// and counted. With `trusted_only`, an entry must also be a trusted
    /// directory, see [`trusted_directory`]; the kept entry is then its
    /// canonical path, and any other entry is dropped and counted rather than
    /// accepted.
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
        let mut entries: Vec<PathBuf> = Vec::new();
        let mut dropped = 0_usize;
        for raw in value.split(SEPARATOR) {
            let mut entry = PathBuf::from(raw);
            if validate_search_directory(&entry).is_err() {
                dropped += 1;
                continue;
            }
            if trusted_only {
                let Some(canonical) = trusted_directory(&entry) else {
                    dropped += 1;
                    continue;
                };
                entry = canonical;
            }
            if entries.contains(&entry) {
                dropped += 1;
            } else {
                entries.push(entry);
            }
        }
        if entries.is_empty() {
            return Err(SearchPathError::NoUsableDirectories);
        }
        Ok(SanitizedPath {
            path: Self::new(entries)?,
            dropped,
        })
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

/// Builds the search path of the existing directories in `table`.
///
/// `~/` entries expand against `home`; without a home they are skipped.
/// Missing or untrusted directories (see [`trusted_directory`]) and entries
/// failing validation are skipped; kept entries are canonical paths.
///
/// # Errors
///
/// Returns [`SearchPathError::NoUsableDirectories`] when no entry exists.
pub fn fallback_search_path(
    table: &[&str],
    home: Option<&Path>,
) -> Result<SearchPath, SearchPathError> {
    let mut entries: Vec<PathBuf> = Vec::new();
    for item in table {
        let entry = match item.strip_prefix(HOME_PREFIX) {
            Some(relative) => match home {
                Some(home) => home.join(relative),
                None => continue,
            },
            None => PathBuf::from(item),
        };
        if validate_search_directory(&entry).is_err() {
            continue;
        }
        if let Some(canonical) = trusted_directory(&entry) {
            if !entries.contains(&canonical) {
                entries.push(canonical);
            }
        }
    }
    if entries.is_empty() {
        return Err(SearchPathError::NoUsableDirectories);
    }
    SearchPath::new(entries)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

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

    /// Creates `path` and its ancestors below the temporary root with mode
    /// 0755 regardless of the process umask.
    fn make_dir(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        fs::create_dir_all(path).expect("dir");
        let root = std::env::temp_dir();
        for directory in path
            .ancestors()
            .take_while(|dir| dir.starts_with(&root) && *dir != root)
        {
            // The temporary directory itself is 0700 and stays so.
            if directory.parent() != Some(root.as_path()) {
                fs::set_permissions(directory, fs::Permissions::from_mode(0o755)).expect("chmod");
            }
        }
    }

    #[test]
    fn sanitize_drops_bad_entries_and_counts_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("b in");
        make_dir(&bin);
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&bin, &link).expect("symlink");
        let canonical = fs::canonicalize(&bin).expect("canonical");
        let value = format!(
            "{}::.:rel:{}:{}:{}",
            bin.display(),
            bin.display(),
            link.display(),
            dir.path().join("gone").display()
        );
        let sanitized = SearchPath::sanitize(&value, true).expect("sanitize");
        // The symlink resolves to the same directory and is a duplicate.
        assert_eq!(sanitized.path.entries(), [canonical]);
        assert_eq!(sanitized.dropped, 6);
        let unchecked = SearchPath::sanitize("/no/such/dir", false).expect("unchecked");
        assert_eq!(unchecked.path.to_env_value(), "/no/such/dir");
    }

    #[test]
    fn untrusted_directories_are_dropped_and_counted() {
        use std::os::unix::fs::PermissionsExt as _;
        if rustix::process::geteuid().is_root() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let good = root.join("opt/homebrew/bin");
        make_dir(&good);
        let mode = |path: &Path, bits: u32| {
            fs::set_permissions(path, fs::Permissions::from_mode(bits)).expect("chmod");
        };
        let group_writable = root.join("group-writable");
        make_dir(&group_writable);
        mode(&group_writable, 0o775);
        let world_writable = root.join("world-writable");
        // An entry below a writable ancestor is refused although it is 0755.
        let below = world_writable.join("inner");
        make_dir(&below);
        mode(&world_writable, 0o777);
        let sticky = root.join("sticky");
        make_dir(&sticky);
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
        assert_eq!(
            sanitized.path.entries(),
            [fs::canonicalize(&good).expect("canonical")]
        );
        assert_eq!(sanitized.dropped, 5);
        assert!(
            trusted_directory(Path::new("/tmp")).is_none(),
            "sticky /tmp"
        );
        let untrusted_only = SearchPath::sanitize(&group_writable.display().to_string(), true);
        assert_eq!(untrusted_only, Err(SearchPathError::NoUsableDirectories));
    }

    #[test]
    fn a_mode_0755_directory_of_ours_is_trusted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        make_dir(&bin);
        assert_eq!(
            trusted_directory(&bin),
            Some(fs::canonicalize(&bin).expect("canonical"))
        );
        assert_eq!(trusted_directory(&dir.path().join("missing")), None);
        let file = dir.path().join("file");
        fs::write(&file, "x").expect("file");
        assert_eq!(trusted_directory(&file), None);
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
