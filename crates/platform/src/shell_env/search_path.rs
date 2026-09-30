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

/// Outcome of sanitizing an untrusted `PATH` value or a fallback table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizedPath {
    /// The usable directories in their original order, each recorded as its
    /// validated canonical path.
    pub path: SearchPath,
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
/// Returns the canonical path the checks ran against. Callers record exactly
/// this path, so validation and recording cover the same directory and nothing
/// can be retargeted after the check. The price is that a profile symlink that
/// later points elsewhere (nix generations) needs a re-record.
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
    /// entries are recorded as their canonical paths, which also detects
    /// duplicates.
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
    /// `extra` is itself a validated [`SearchPath`], so every appended entry
    /// already satisfies the entry rules.
    ///
    /// Directories are appended one by one while the joined value fits
    /// [`MAX_SEARCH_PATH_BYTES`]; the rest are left out.
    #[must_use]
    pub fn with_appended(&self, extra: &Self) -> Self {
        let mut entries = self.entries.clone();
        for entry in extra.entries() {
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
                    entry.clone_from(&canonical);
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
/// their canonical paths.
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
        // The symlink resolves to the same directory and is a duplicate.
        assert_eq!(sanitized.path.entries(), [bin]);
        assert_eq!(sanitized.ignored, 6);
        assert!(sanitized.untrusted.is_empty());
        let unchecked = SearchPath::sanitize("/no/such/dir", false).expect("unchecked");
        assert_eq!(unchecked.path.to_env_value(), "/no/such/dir");
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

    #[test]
    fn a_symlink_entry_is_recorded_as_its_canonical_target() {
        let dir = fixture();
        let root = dir.path();
        let good = root.join("good/bin");
        make_dir(root, &good);
        let link = root.join("current");
        std::os::unix::fs::symlink(root.join("good"), &link).expect("symlink");
        let sanitized =
            SearchPath::sanitize(&link.join("bin").display().to_string(), true).expect("ok");
        assert_eq!(sanitized.path.entries(), std::slice::from_ref(&good));
        // Retargeting the link afterwards cannot change the recorded entry.
        let evil = root.join("evil/bin");
        make_dir(root, &evil);
        fs::remove_file(&link).expect("unlink");
        std::os::unix::fs::symlink(root.join("evil"), &link).expect("retarget");
        assert_eq!(sanitized.path.entries(), [good]);
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
        let extra = SearchPath::new(vec!["/b".into(), "/c".into()]).expect("extra");
        assert_eq!(base.with_appended(&extra).to_env_value(), "/a:/b:/c");
        let long = PathBuf::from(format!("/{}", "x".repeat(MAX_SEARCH_PATH_BYTES - 4)));
        let extra = SearchPath::new(vec![long, "/d".into()]).expect("extra");
        assert_eq!(base.with_appended(&extra).to_env_value(), "/a:/b:/d");
    }

    #[test]
    fn every_constructor_enforces_the_entry_rules() {
        // `new` and `sanitize` are the only ways to build a non-empty path, and
        // `with_appended` accepts only another validated path, so a rejected
        // form can never reach the joined value.
        for bad in ["", ".", "rel", "/a:b", "/a\u{1}", "/a/../b"] {
            assert!(
                SearchPath::new(vec![PathBuf::from(bad)]).is_err(),
                "{bad:?}"
            );
        }
        assert!(SearchPath::default().is_empty());
        assert!(SearchPath::empty()
            .with_appended(&SearchPath::default())
            .is_empty());
        let appended =
            SearchPath::empty().with_appended(&SearchPath::new(vec!["/ok".into()]).expect("path"));
        assert_eq!(appended.to_env_value(), "/ok");
        assert!(!appended.to_env_value().ends_with(':'));
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
