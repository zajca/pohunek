//! Validated executable search paths and the built-in fallback directories.

use std::path::{Path, PathBuf};

use thiserror::Error;

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
    /// Entries that are empty, relative, `.`-style, duplicated, or (with
    /// `existing_only`) not existing directories are dropped and counted. A
    /// directory that is a symlink to a directory counts as existing.
    ///
    /// # Errors
    ///
    /// Returns [`SearchPathError::ControlCharacter`] when the value holds a
    /// control character at all (the value is then garbage, not a `PATH`),
    /// [`SearchPathError::NoUsableDirectories`] when nothing survives, and
    /// [`SearchPathError::TooLong`] when the survivors exceed the bound.
    pub fn sanitize(value: &str, existing_only: bool) -> Result<SanitizedPath, SearchPathError> {
        if value.chars().any(char::is_control) {
            return Err(SearchPathError::ControlCharacter);
        }
        let mut entries: Vec<PathBuf> = Vec::new();
        let mut dropped = 0_usize;
        for raw in value.split(SEPARATOR) {
            let entry = PathBuf::from(raw);
            let usable = validate_search_directory(&entry).is_ok()
                && !entries.contains(&entry)
                && (!existing_only || entry.is_dir());
            if usable {
                entries.push(entry);
            } else {
                dropped += 1;
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
/// Missing directories and entries failing validation are skipped.
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
        if validate_search_directory(&entry).is_ok() && entry.is_dir() && !entries.contains(&entry)
        {
            entries.push(entry);
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

    #[test]
    fn sanitize_drops_bad_entries_and_counts_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("b in");
        fs::create_dir(&bin).expect("bin");
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
        assert_eq!(sanitized.path.entries(), [bin.clone(), link]);
        assert_eq!(sanitized.dropped, 5);
        let unchecked = SearchPath::sanitize("/no/such/dir", false).expect("unchecked");
        assert_eq!(unchecked.path.to_env_value(), "/no/such/dir");
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
