//! Rewrites the package pin of a host agent profile.
//!
//! The pin is the `package` and `digest` keys of `<agents>/<name>.toml`. The
//! rewrite edits exactly those two keys and leaves every other byte alone, and
//! the new file replaces the old one with a single `rename(2)`: the profile
//! name resolves to either the old or the new complete file at every instant,
//! so a concurrent resolution never sees a missing profile.
//!
//! Profile files can hold secret `[env]` values. Nothing here prints, logs or
//! returns file content: a diagnostic names a key or a line number only.

// Rust guideline compliant 2026-10-05

use std::fmt::Write as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::PathBuf;

use package::PackageDigest;
use pohunek_platform::filesystem::{
    AtomicReplaceError, EntryIdentity, EntryKind, FsError, StageOutcome, TrustedDir,
};
use protocol::{PackageId, RuntimeId};
use toml_edit::{DocumentMut, Item, TomlError, Value};
use tracing::warn;

use super::{read_profile_text, ProfileRegistry, MAX_PROFILE_BYTES, MAX_SCANNED_PROFILES};
use crate::project::config::validate_name;

/// Extension of a profile file.
const PROFILE_EXTENSION: &str = ".toml";

/// Permission bits of a published profile, at most.
///
/// A profile can hold secret `[env]` values, so the rewrite never leaves it
/// readable or writable by anyone but its owner, even if the original was.
const PUBLISH_MODE_CEILING: u32 = 0o600;

/// Permission bits of a file or directory that let another account modify it.
const OTHERS_WRITE_BITS: u32 = 0o022;

/// Permission bits of a file mode.
const MODE_BITS: u32 = 0o7777;

/// Prefix of the temporary file a rewrite stages.
///
/// The name carries no `.toml` extension, so the profile loader and the
/// retention scan, which read `*.toml` files only, never mistake it for a
/// profile.
const TEMPORARY_PREFIX: &str = ".pohunek-profile-bind-";

/// Random bytes in a temporary file name.
const TEMPORARY_RANDOM_BYTES: usize = 16;

/// The explicit pin a profile carries for a package-served base runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pin {
    pub(crate) package: PackageId,
    pub(crate) digest: PackageDigest,
}

/// The keys of a profile the bind reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProfileHead {
    pub(crate) base: RuntimeId,
    pub(crate) package: Option<PackageId>,
    pub(crate) digest: Option<PackageDigest>,
}

impl ProfileHead {
    /// The pin, when both keys are present.
    #[must_use]
    pub(crate) fn pin(&self) -> Option<Pin> {
        Some(Pin {
            package: self.package.clone()?,
            digest: self.digest.clone()?,
        })
    }
}

/// Why a profile could not be read or rewritten.
///
/// The detail of [`Self::Unusable`] and [`Self::Failed`] names keys, lines and
/// reasons only, so it is safe to log.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BindError {
    /// The host has no such profile.
    NotFound,
    /// The profile fails the acceptance rule, is a link, or is not rewritable
    /// TOML.
    Unusable(String),
    /// The profile changed after it was read; the file was left as found.
    Changed,
    /// A filesystem operation failed.
    Failed(&'static str),
}

/// Escape control characters of text that came from a parser.
fn escape_controls(text: &str) -> String {
    text.chars()
        .flat_map(|character| {
            if character.is_control() {
                character.escape_default().collect::<Vec<_>>()
            } else {
                vec![character]
            }
        })
        .collect()
}

/// Describe a TOML parse failure by message and line only.
///
/// The error's own `Display` quotes the offending source line, which can be an
/// `[env]` entry carrying a secret.
fn edit_diagnostic(content: &str, error: &TomlError) -> String {
    let message = escape_controls(error.message());
    match error.span() {
        Some(span) => {
            let line = content
                .get(..span.start)
                .map_or(1, |head| head.matches('\n').count() + 1);
            format!("line {line}: {message}")
        }
        None => message,
    }
}

/// Read `base`, `package` and `digest` from profile text.
///
/// Diagnostics name keys and lines, never values.
pub(crate) fn parse_head(text: &str) -> Result<ProfileHead, String> {
    let document: DocumentMut = text
        .parse()
        .map_err(|error: TomlError| edit_diagnostic(text, &error))?;
    let base = string_key(&document, "base")?.ok_or_else(|| "`base` is missing".to_owned())?;
    let base = RuntimeId::parse(base).map_err(|_invalid| "`base` is not a valid runtime id")?;
    let package = string_key(&document, "package")?
        .map(PackageId::parse)
        .transpose()
        .map_err(|_invalid| "`package` is not a valid package id")?;
    let digest = string_key(&document, "digest")?
        .map(PackageDigest::parse)
        .transpose()
        .map_err(|_invalid| "`digest` is not a valid package digest")?;
    Ok(ProfileHead {
        base,
        package,
        digest,
    })
}

fn string_key<'a>(document: &'a DocumentMut, key: &str) -> Result<Option<&'a str>, String> {
    match document.get(key) {
        None => Ok(None),
        Some(item) => item
            .as_str()
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be a string")),
    }
}

/// The text of `original` with the two pin keys removed.
fn without_pin(original: &str) -> Result<String, String> {
    let mut document: DocumentMut = original
        .parse()
        .map_err(|error: TomlError| edit_diagnostic(original, &error))?;
    document.remove("package");
    document.remove("digest");
    Ok(document.to_string())
}

/// Set the `package` and `digest` keys of `text` and return the new text.
///
/// Existing pin keys keep their position and decoration. Missing ones are
/// inserted directly after `base`. Refuses to return text whose content other
/// than the two keys differs from `text`.
pub(crate) fn apply_pin(text: &str, pin: &Pin) -> Result<String, String> {
    let mut document: DocumentMut = text
        .parse()
        .map_err(|error: TomlError| edit_diagnostic(text, &error))?;
    let root = document.as_table_mut();
    let wanted = [
        ("package", pin.package.as_str()),
        ("digest", pin.digest.as_str()),
    ];
    let mut missing = Vec::new();
    for (key, value) in wanted {
        match root.get_mut(key) {
            Some(Item::Value(existing)) if existing.is_str() => {
                let mut replacement = Value::from(value);
                *replacement.decor_mut() = existing.decor().clone();
                *existing = replacement;
            }
            Some(_) => return Err(format!("`{key}` must be a string")),
            None => missing.push((key, value)),
        }
    }
    if !missing.is_empty() {
        // Plain tables are emitted by position, but values and dotted tables
        // are emitted in map order, so everything after `base` is lifted out,
        // the new keys go in, and the lifted entries return in their order.
        let after_base: Vec<String> = root
            .iter()
            .skip_while(|(key, _)| *key != "base")
            .skip(1)
            .filter(|(_, item)| match item {
                Item::Value(_) => true,
                Item::Table(table) => table.is_dotted(),
                Item::None | Item::ArrayOfTables(_) => false,
            })
            .map(|(key, _)| key.to_owned())
            .collect();
        let lifted: Vec<_> = after_base
            .iter()
            .filter_map(|key| root.remove_entry(key))
            .collect();
        for (key, value) in missing {
            root.insert(key, Item::Value(Value::from(value)));
        }
        for (key, item) in lifted {
            root.insert_formatted(&key, item);
        }
    }
    let rewritten = document.to_string();
    if without_pin(&rewritten)? != without_pin(text)? {
        return Err("the rewrite would change content other than the pin".to_owned());
    }
    Ok(rewritten)
}

/// A profile file as read for a rewrite.
#[derive(Debug)]
pub(crate) struct ProfileFile {
    text: String,
    mode: u32,
    identity: EntryIdentity,
}

impl ProfileFile {
    /// The text of the file.
    #[must_use]
    pub(crate) fn text(&self) -> &str {
        &self.text
    }
}

/// The agents directory opened for a rewrite.
#[derive(Debug)]
pub(crate) struct BindDir {
    trusted: TrustedDir,
    root: PathBuf,
}

impl ProfileRegistry {
    /// Opens the agents directory for a rewrite.
    ///
    /// The directory must be the one the registry loads profiles from, owned
    /// by the daemon user and not group- or world-writable.
    ///
    /// # Errors
    ///
    /// Returns [`BindError::NotFound`] when the host loads no profiles from a
    /// directory or the directory does not exist, and [`BindError::Unusable`]
    /// when it fails the owner-safety check.
    pub(crate) fn open_for_bind(&self) -> Result<BindDir, BindError> {
        let Some(root) = self.dir.as_ref() else {
            return Err(BindError::NotFound);
        };
        match TrustedDir::open_absolute_owner_safe(root, OTHERS_WRITE_BITS) {
            Ok(trusted) => Ok(BindDir {
                trusted,
                root: root.clone(),
            }),
            Err(error) if error.io_kind() == Some(std::io::ErrorKind::NotFound) => {
                Err(BindError::NotFound)
            }
            Err(error) => Err(BindError::Unusable(format!(
                "the agents directory is not owner-safe: {error}"
            ))),
        }
    }
}

fn profile_file_name(name: &str) -> String {
    format!("{name}{PROFILE_EXTENSION}")
}

fn max_profile_bytes() -> usize {
    usize::try_from(MAX_PROFILE_BYTES).unwrap_or(usize::MAX)
}

impl BindDir {
    /// Removes the temporary files an interrupted rewrite left behind.
    ///
    /// A temporary never has a profile name, so a stale one is inert; removal
    /// only keeps secret `[env]` values from lingering in it. Failures are
    /// logged and skipped.
    pub(crate) fn remove_stale_temporaries(&self) {
        let names = match self.trusted.entry_names_limited(MAX_SCANNED_PROFILES) {
            Ok((names, _truncated)) => names,
            Err(error) => {
                warn!(%error, "the agents directory could not be listed for stale temporaries");
                return;
            }
        };
        for name in names {
            if !name.to_string_lossy().starts_with(TEMPORARY_PREFIX) {
                continue;
            }
            let removed = self
                .trusted
                .entry_identity(&name, EntryKind::RegularFile)
                .and_then(|identity| match identity {
                    Some(identity) => {
                        match self
                            .trusted
                            .stage_random(&name, TEMPORARY_PREFIX, identity)?
                        {
                            StageOutcome::Staged(entry) => entry.remove().map(|_outcome| ()),
                            _ => Ok(()),
                        }
                    }
                    None => Ok(()),
                });
            if let Err(error) = removed {
                warn!(%error, "a stale profile rewrite temporary was not removed");
            }
        }
    }

    /// Reads the profile `name` through the daemon's shared acceptance rule.
    ///
    /// A symbolic link or a hard-linked file is refused, because the rewrite
    /// renames over the name and would replace the link rather than the
    /// content.
    ///
    /// # Errors
    ///
    /// Returns [`BindError::NotFound`] for a missing profile and
    /// [`BindError::Unusable`] for one that fails the rule or is a link.
    pub(crate) fn read(&self, name: &str) -> Result<ProfileFile, BindError> {
        if validate_name("agent", name).is_err() {
            return Err(BindError::NotFound);
        }
        let file = profile_file_name(name);
        let path = self.root.join(&file);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(BindError::NotFound)
            }
            Err(_error) => return Err(BindError::Failed("the profile could not be inspected")),
        };
        if !metadata.file_type().is_file() {
            return Err(BindError::Unusable(
                "the profile is a symbolic link or not a regular file".to_owned(),
            ));
        }
        let mode = metadata.mode() & MODE_BITS;
        let identity = match self.trusted.entry_identity(&file, EntryKind::RegularFile) {
            Ok(Some(identity)) => identity,
            Ok(None) => return Err(BindError::NotFound),
            Err(error) => {
                return Err(BindError::Unusable(format!(
                    "the profile fails the file safety checks: {error}"
                )))
            }
        };
        let text = read_profile_text(&self.root, name, &path)
            .map_err(|error| BindError::Unusable(error.msg))?;
        Ok(ProfileFile {
            text,
            mode,
            identity,
        })
    }

    /// Whether the profile file is still exactly `original`: the same inode,
    /// mode and content.
    fn is_unchanged(&self, name: &str, original: &ProfileFile) -> Result<(), FsError> {
        let file = profile_file_name(name);
        let changed = || FsError::IdentityChanged {
            path: self.root.join(&file),
        };
        let metadata =
            std::fs::symlink_metadata(self.root.join(&file)).map_err(|_error| changed())?;
        let mode = metadata.mode() & MODE_BITS;
        if mode != original.mode {
            return Err(changed());
        }
        let identity =
            self.trusted
                .entry_identity_with_mode(&file, EntryKind::RegularFile, mode)?;
        if identity != Some(original.identity) {
            return Err(changed());
        }
        let bytes = self.trusted.read_file(&file, mode, max_profile_bytes())?;
        if bytes != original.text.as_bytes() {
            return Err(changed());
        }
        Ok(())
    }

    /// Replaces the profile `name` with `text` atomically.
    ///
    /// The text is written to a temporary file in the directory and renamed
    /// over `<name>.toml`, so the name never stops resolving. Immediately
    /// before the rename the file is compared with `original`; a profile that
    /// changed since it was read is left as found.
    ///
    /// # Errors
    ///
    /// Returns [`BindError::Changed`] when the profile changed after it was
    /// read and [`BindError::Failed`] when the replacement failed or its
    /// durability is uncertain; in the first case and for a failure before the
    /// rename the original file is untouched.
    pub(crate) fn publish(
        &self,
        name: &str,
        original: &ProfileFile,
        text: &str,
    ) -> Result<(), BindError> {
        let file = profile_file_name(name);
        let mut random = [0_u8; TEMPORARY_RANDOM_BYTES];
        getrandom::getrandom(&mut random)
            .map_err(|_error| BindError::Failed("the system random source failed"))?;
        let mut temporary = String::from(TEMPORARY_PREFIX);
        for byte in random {
            let _ = write!(temporary, "{byte:02x}");
        }
        let mode = original.mode & PUBLISH_MODE_CEILING;
        self.trusted
            .replace_file_checked(&file, &temporary, text.as_bytes(), mode, || {
                self.is_unchanged(name, original)
            })
            .map_err(|error| match error {
                AtomicReplaceError::BeforeCommit(FsError::IdentityChanged { .. }) => {
                    BindError::Changed
                }
                AtomicReplaceError::BeforeCommit(error) => {
                    warn!(%error, "the profile could not be replaced and is unchanged");
                    BindError::Failed("the profile could not be replaced; it is unchanged")
                }
                AtomicReplaceError::CommittedDurabilityUncertain(error) => {
                    warn!(%error, "the profile was replaced but the directory could not be synchronized");
                    BindError::Failed(
                        "the profile was replaced but the directory could not be synchronized",
                    )
                }
                _ => BindError::Failed("the profile could not be replaced"),
            })
    }
}

#[cfg(test)]
mod tests;
