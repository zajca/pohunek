//! `pohunek plugin profile`: pin host agent profiles to installed packages.
//!
//! A host agent profile (`<config_dir>/agents/<name>.toml`) extends a base
//! runtime. When an installed package serves that runtime, the profile must
//! carry an explicit `package` and `digest` pin. A profile moves to another
//! digest only through `migrate`, which rewrites exactly those two keys and
//! leaves every other byte of the file alone.
//!
//! Profile files can hold secret `[env]` values. Nothing here deserializes
//! `[env]`, prints file content, or echoes a parse error's source line: a
//! diagnostic names a key or a line number only.

// Rust guideline compliant 2026-10-04

use std::fmt::Write as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use clap::{Args, Subcommand};
use pohunek_platform::filesystem::{
    DestinationExpectation, DisplacingReplaceError, EntryIdentity, EntryKind, FsError, StagedEntry,
    TrustedDir,
};
use protocol::{PackageDigest, PackageId, PackageInfo, PackageVersion, RuntimeId};
use serde::Serialize;
use toml_edit::{DocumentMut, Item, TomlError, Value};

use super::plugin::{
    list, print_output, render_table, sanitize, short_digest, state_label, summary, DigestSelector,
    Error,
};
use crate::client::Client;
use crate::error::CliError;
use crate::paths::Paths;
use crate::target::LOCAL_HOST;

/// Directory under pohunek's config dir that holds host agent profiles.
pub(crate) const AGENTS_DIR: &str = "agents";

/// Extension of a profile file.
const PROFILE_EXTENSION: &str = ".toml";

/// Longest accepted profile name in bytes.
///
/// Filesystems cap a file name at 255 bytes; the `.toml` extension takes five,
/// so no longer name can exist as a profile file. The daemon sets no tighter
/// bound, and a tighter one here would hide profiles it can load.
const PROFILE_NAME_MAX_BYTES: usize = 250;

/// Most directory entries one listing or completion reads.
///
/// A host holds a handful of profiles; the bound keeps a directory flooded
/// with files from stalling the command or a shell completion.
pub(crate) const MAX_PROFILE_ENTRIES: usize = 1024;

/// Largest profile file read, in bytes.
///
/// Real profiles are a few hundred bytes. The bound keeps a hostile or runaway
/// file from exhausting memory before it is parsed.
const MAX_PROFILE_BYTES: usize = 1 << 20;

/// Permission bits a rewritten profile may keep.
///
/// A profile can hold secret `[env]` values, so the rewrite never leaves it
/// readable or writable by anyone but its owner, even if the original was.
const REWRITE_MODE_CEILING: u32 = 0o600;

/// Permission bits that let another account modify a file.
const OTHERS_WRITE_BITS: u32 = 0o022;

/// Permission bits of a file mode.
const MODE_BITS: u32 = 0o7777;

/// Prefix of the temporary file a rewrite stages.
///
/// It has no `.toml` extension and starts with a dot, so neither the daemon's
/// profile enumeration nor `profile list` can mistake it for a profile.
const TEMPORARY_PREFIX: &str = ".pohunek-profile-migrate-";

/// Random bytes in a temporary file name.
const TEMPORARY_RANDOM_BYTES: usize = 16;

/// `pohunek plugin profile` subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum ProfileAction {
    /// List host agent profiles and whether each needs a package pin.
    ///
    /// Reads `agents/*.toml` under the pohunek config directory. States:
    /// `builtin` (nothing to do), `pinned`, `needs_migration` (an installed
    /// package serves the base runtime but the profile pins none),
    /// `pin_not_installed` (the pinned digest is not installed) and
    /// `unreadable`.
    List {
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
    },

    /// Pin a profile to an installed package, rewriting only its `package`
    /// and `digest` keys.
    ///
    /// Without `--digest` the profile moves to the selected, enabled package
    /// that serves its base runtime. Nothing is rewritten until you repeat the
    /// command with `--yes`. A profile never changes digest through `plugin
    /// update` or `plugin select`.
    Migrate(MigrateArgs),
}

impl ProfileAction {
    /// Whether the subcommand requested `--json` output.
    pub(crate) fn wants_json(&self) -> bool {
        match self {
            Self::List { json } => *json,
            Self::Migrate(args) => args.json,
        }
    }
}

/// Arguments of `pohunek plugin profile migrate`.
#[derive(Debug, Clone, Args)]
pub(crate) struct MigrateArgs {
    /// Profile name: the file name under `agents/` without `.toml`.
    #[arg(value_name = "NAME", value_parser = parse_profile_name)]
    name: String,
    /// Pin to the installed package with this digest: the full
    /// `sha256:<64 hex>` or a unique hex prefix of at least 12 characters.
    #[arg(long, value_name = "DIGEST")]
    digest: Option<DigestSelector>,
    /// Consent to the rewrite after reviewing the change.
    #[arg(long)]
    yes: bool,
    /// Emit machine-readable JSON instead of text.
    #[arg(long)]
    json: bool,
}

/// Whether `name` is a valid profile name.
///
/// Mirrors the daemon's single-segment charset guard (`validate_name` in
/// `crates/daemon/src/project/config.rs`): non-empty, only ASCII letters,
/// digits, `.`, `_` and `-`, no leading `.` or `-`, and no `..`.
pub(crate) fn is_valid_profile_name(name: &str) -> bool {
    let Some(first) = name.chars().next() else {
        return false;
    };
    name.len() <= PROFILE_NAME_MAX_BYTES
        && first != '.'
        && first != '-'
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn parse_profile_name(value: &str) -> Result<String, String> {
    if is_valid_profile_name(value) {
        Ok(value.to_owned())
    } else {
        Err(
            "expected letters, digits, `.`, `_` and `-`, with no leading `.` or `-` and no `..`"
                .to_owned(),
        )
    }
}

/// Profile names present in the agents directory, for shell completion.
///
/// Reads names only, never file content, and returns nothing on any failure.
pub(crate) fn profile_names(agents_dir: &Path) -> Vec<String> {
    let Ok(Some(dir)) = open_agents_dir(agents_dir) else {
        return Vec::new();
    };
    let Ok((names, _truncated)) = dir.entry_names_limited(MAX_PROFILE_ENTRIES) else {
        return Vec::new();
    };
    let mut names: Vec<String> = names
        .iter()
        .filter_map(|name| name.to_str()?.strip_suffix(PROFILE_EXTENSION))
        .filter(|stem| is_valid_profile_name(stem))
        .map(str::to_owned)
        .collect();
    names.sort();
    names
}

/// The explicit pin a profile carries for a package-served base runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pin {
    package: PackageId,
    digest: PackageDigest,
}

/// The keys of a profile the commands read.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProfileHead {
    base: RuntimeId,
    package: Option<PackageId>,
    digest: Option<PackageDigest>,
}

impl ProfileHead {
    fn pin(&self) -> Option<Pin> {
        Some(Pin {
            package: self.package.clone()?,
            digest: self.digest.clone()?,
        })
    }
}

/// Describe a TOML parse failure by message and line only.
///
/// The error's own `Display` quotes the offending source line, which can be an
/// `[env]` entry carrying a secret.
fn toml_diagnostic(content: &str, error: &TomlError) -> String {
    let message = sanitize(error.message());
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
fn parse_head(text: &str) -> Result<ProfileHead, String> {
    let document: DocumentMut = text
        .parse()
        .map_err(|error: TomlError| toml_diagnostic(text, &error))?;
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
        .map_err(|error: TomlError| toml_diagnostic(original, &error))?;
    document.remove("package");
    document.remove("digest");
    Ok(document.to_string())
}

/// Set the `package` and `digest` keys of `text` and return the new text.
///
/// Existing pin keys keep their position and decoration. Missing ones are
/// inserted directly after `base`. Refuses to return text whose content other
/// than the two keys differs from `text`.
fn apply_pin(text: &str, pin: &Pin) -> Result<String, String> {
    let mut document: DocumentMut = text
        .parse()
        .map_err(|error: TomlError| toml_diagnostic(text, &error))?;
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

/// State of a profile relative to the installed packages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ProfileState {
    /// No installed package serves the base runtime; nothing to do.
    Builtin,
    /// The profile pins an installed package that serves its base runtime.
    Pinned,
    /// An installed package serves the base runtime and the pin is missing,
    /// incomplete or points at another package.
    NeedsMigration,
    /// The pinned digest is not installed.
    PinNotInstalled,
    /// The file cannot be read or parsed.
    Unreadable,
}

impl ProfileState {
    fn label(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Pinned => "pinned",
            Self::NeedsMigration => "needs_migration",
            Self::PinNotInstalled => "pin_not_installed",
            Self::Unreadable => "unreadable",
        }
    }
}

/// Classify a readable profile against the installed packages.
fn classify(head: &ProfileHead, packages: &[PackageInfo]) -> (ProfileState, Option<&'static str>) {
    let serves_base = |info: &PackageInfo| info.runtime_id.as_ref() == Some(&head.base);
    match (&head.package, &head.digest) {
        (Some(package), Some(digest)) => {
            match packages.iter().find(|info| &info.digest == digest) {
                None => (ProfileState::PinNotInstalled, None),
                Some(info) if &info.package.id == package && serves_base(info) => {
                    (ProfileState::Pinned, None)
                }
                Some(_) => (
                    ProfileState::NeedsMigration,
                    Some("the pinned package does not serve the base runtime"),
                ),
            }
        }
        (None, None) => {
            if packages.iter().any(serves_base) {
                (ProfileState::NeedsMigration, None)
            } else {
                (ProfileState::Builtin, None)
            }
        }
        _ => (
            ProfileState::NeedsMigration,
            Some("`package` and `digest` must be set together"),
        ),
    }
}

/// One profile in a listing.
#[derive(Debug, Serialize)]
struct ProfileEntry {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<RuntimeId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    package: Option<PackageId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    digest: Option<PackageDigest>,
    state: ProfileState,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

/// Result of `profile list`.
#[derive(Debug, Serialize)]
struct ProfileListing {
    profiles: Vec<ProfileEntry>,
    /// Whether the directory held more entries than were read.
    truncated: bool,
}

/// Why a profile file could not be loaded.
#[derive(Debug, PartialEq, Eq)]
enum LoadFault {
    /// No such file.
    Missing,
    /// The file fails the ownership, type or permission policy.
    Unsafe(&'static str),
    /// The file could not be read.
    Unreadable(&'static str),
}

impl LoadFault {
    fn detail(&self) -> &'static str {
        match self {
            Self::Missing => "the file does not exist",
            Self::Unsafe(reason) | Self::Unreadable(reason) => reason,
        }
    }
}

/// Kind of a directory entry as the policy distinguishes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileKind {
    Regular,
    Symlink,
    Other,
}

/// Apply the profile-file policy the daemon enforces to its profiles: a
/// regular file owned by the current user and not group- or world-writable.
/// A symbolic link is refused because the daemon would execute its target.
fn check_file_policy(
    kind: FileKind,
    owner: u32,
    effective_user: u32,
    mode: u32,
) -> Result<(), &'static str> {
    match kind {
        FileKind::Symlink => return Err("the file is a symbolic link"),
        FileKind::Other => return Err("the file is not a regular file"),
        FileKind::Regular => {}
    }
    if owner != effective_user {
        return Err("the file is not owned by the current user");
    }
    if mode & OTHERS_WRITE_BITS != 0 {
        return Err("the file is group- or world-writable");
    }
    Ok(())
}

/// A profile file read through the trusted directory descriptor.
struct LoadedProfile {
    text: String,
    mode: u32,
    identity: EntryIdentity,
}

fn profile_file_name(name: &str) -> String {
    format!("{name}{PROFILE_EXTENSION}")
}

/// Open the agents directory; `None` when it does not exist.
///
/// The directory must be owned by the current user and not group- or
/// world-writable, the daemon's own gate for host configuration.
fn open_agents_dir(path: &Path) -> Result<Option<TrustedDir>, FsError> {
    match TrustedDir::open_absolute_owner_safe(path, OTHERS_WRITE_BITS) {
        Ok(dir) => Ok(Some(dir)),
        Err(error) if error.io_kind() == Some(std::io::ErrorKind::NotFound) => Ok(None),
        Err(error) => Err(error),
    }
}

fn load_profile(dir: &TrustedDir, name: &str) -> Result<LoadedProfile, LoadFault> {
    let file = profile_file_name(name);
    let metadata = match std::fs::symlink_metadata(dir.path().join(&file)) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(LoadFault::Missing)
        }
        Err(_error) => return Err(LoadFault::Unreadable("the file could not be inspected")),
    };
    let file_type = metadata.file_type();
    let kind = if file_type.is_symlink() {
        FileKind::Symlink
    } else if file_type.is_file() {
        FileKind::Regular
    } else {
        FileKind::Other
    };
    let mode = metadata.mode() & MODE_BITS;
    check_file_policy(
        kind,
        metadata.uid(),
        nix::unistd::Uid::effective().as_raw(),
        mode,
    )
    .map_err(LoadFault::Unsafe)?;
    // The descriptor-level checks below revalidate type, owner, mode and link
    // count on the opened file, so a swap after the inspection fails closed.
    let identity = dir
        .entry_identity_with_mode(&file, EntryKind::RegularFile, mode)
        .map_err(|_error| LoadFault::Unreadable("the file failed the safety checks"))?
        .ok_or(LoadFault::Missing)?;
    let bytes = dir
        .read_file(&file, mode, MAX_PROFILE_BYTES)
        .map_err(|error| match error {
            FsError::FileTooLarge { .. } => LoadFault::Unreadable("the file is too large"),
            _ => LoadFault::Unreadable("the file could not be read safely"),
        })?;
    let text = String::from_utf8(bytes)
        .map_err(|_error| LoadFault::Unreadable("the file is not valid UTF-8"))?;
    Ok(LoadedProfile {
        text,
        mode,
        identity,
    })
}

fn directory_error(error: &FsError) -> Error {
    Error::ProfileDirectory {
        detail: sanitize(&error.to_string()),
    }
}

/// Build the listing of every profile file in `dir`.
fn list_profiles(
    dir: Option<&TrustedDir>,
    packages: &[PackageInfo],
) -> Result<ProfileListing, Error> {
    let Some(dir) = dir else {
        return Ok(ProfileListing {
            profiles: Vec::new(),
            truncated: false,
        });
    };
    let (names, truncated) = dir
        .entry_names_limited(MAX_PROFILE_ENTRIES)
        .map_err(|error| directory_error(&error))?;
    let mut profiles = Vec::new();
    for entry in &names {
        let entry = entry.to_string_lossy();
        let Some(stem) = entry.strip_suffix(PROFILE_EXTENSION) else {
            continue;
        };
        profiles.push(profile_entry(dir, stem, packages));
    }
    profiles.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(ProfileListing {
        profiles,
        truncated,
    })
}

fn unreadable_entry(name: &str, detail: String) -> ProfileEntry {
    ProfileEntry {
        name: sanitize(name),
        base: None,
        package: None,
        digest: None,
        state: ProfileState::Unreadable,
        detail: Some(detail),
    }
}

fn profile_entry(dir: &TrustedDir, name: &str, packages: &[PackageInfo]) -> ProfileEntry {
    if !is_valid_profile_name(name) {
        return unreadable_entry(name, "not a valid profile name".to_owned());
    }
    let loaded = match load_profile(dir, name) {
        Ok(loaded) => loaded,
        Err(fault) => return unreadable_entry(name, fault.detail().to_owned()),
    };
    match parse_head(&loaded.text) {
        Err(detail) => unreadable_entry(name, detail),
        Ok(head) => {
            let (state, detail) = classify(&head, packages);
            ProfileEntry {
                name: name.to_owned(),
                base: Some(head.base),
                package: head.package,
                digest: head.digest,
                state,
                detail: detail.map(str::to_owned),
            }
        }
    }
}

fn render_listing(listing: &ProfileListing) -> String {
    if listing.profiles.is_empty() {
        return "No host agent profiles.\n".to_owned();
    }
    let rows: Vec<[String; 5]> = listing
        .profiles
        .iter()
        .map(|entry| {
            let state = entry.detail.as_deref().map_or_else(
                || entry.state.label().to_owned(),
                |detail| format!("{} ({detail})", entry.state.label()),
            );
            [
                entry.name.clone(),
                entry
                    .base
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), ToString::to_string),
                entry
                    .package
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), ToString::to_string),
                entry
                    .digest
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), short_digest),
                state,
            ]
        })
        .collect();
    let mut output = render_table(["NAME", "BASE", "PACKAGE", "DIGEST", "STATE"], &rows);
    if listing.truncated {
        let _ = writeln!(
            output,
            "More than {MAX_PROFILE_ENTRIES} entries; the rest are not shown."
        );
    }
    output
}

/// Pick the package a profile migrates to.
///
/// With a selector, the installed package that matches it and serves `base`.
/// Without one, the one selected and enabled package that serves `base`.
fn resolve_target<'a>(
    name: &str,
    base: &RuntimeId,
    packages: &'a [PackageInfo],
    selector: Option<&DigestSelector>,
) -> Result<&'a PackageInfo, Error> {
    let target = |detail: String| Error::ProfileTarget {
        name: name.to_owned(),
        detail,
    };
    let serving: Vec<&PackageInfo> = packages
        .iter()
        .filter(|info| info.runtime_id.as_ref() == Some(base))
        .collect();
    if serving.is_empty() {
        return Err(Error::ProfileBaseBuiltin {
            name: name.to_owned(),
            base: base.to_string(),
        });
    }
    let candidates: Vec<&PackageInfo> = match selector {
        Some(selector) => serving
            .into_iter()
            .filter(|info| selector.matches(&info.digest))
            .collect(),
        None => serving
            .into_iter()
            .filter(|info| info.selected && info.enabled)
            .collect(),
    };
    let chosen = match candidates.as_slice() {
        [] if selector.is_some() => {
            return Err(target(format!(
                "no installed package with that digest serves base runtime {base}"
            )))
        }
        [] => {
            return Err(target(format!(
                "no selected, enabled package serves base runtime {base}; select one with `pohunek plugin select` or pass --digest"
            )))
        }
        [only] => *only,
        several => {
            let listed: Vec<String> = several
                .iter()
                .map(|info| summary(&info.package.id, &info.package.version, &info.digest))
                .collect();
            return Err(target(format!(
                "several packages match for base runtime {base}: {}; pass a longer --digest",
                listed.join(", ")
            )));
        }
    };
    if chosen.fault.is_some() {
        return Err(target(format!(
            "{} is faulted; run `pohunek plugin doctor` first",
            summary(&chosen.package.id, &chosen.package.version, &chosen.digest)
        )));
    }
    Ok(chosen)
}

/// Whether the profile already carries exactly the target pin.
fn already_pinned(head: &ProfileHead, target: &PackageInfo) -> bool {
    head.package.as_ref() == Some(&target.package.id)
        && head.digest.as_ref() == Some(&target.digest)
}

/// How a `migrate` ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MigrationStatus {
    Migrated,
    Unchanged,
}

/// Result of `profile migrate`.
#[derive(Debug, Serialize)]
struct MigrationResult {
    name: String,
    base: RuntimeId,
    package: PackageId,
    version: PackageVersion,
    digest: PackageDigest,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_package: Option<PackageId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_digest: Option<PackageDigest>,
    status: MigrationStatus,
}

fn render_review(name: &str, head: &ProfileHead, target: &PackageInfo) -> String {
    let mut output = String::from("The profile would change:\n");
    let _ = writeln!(output, "Profile:     {name}");
    let _ = writeln!(output, "Base:        {}", head.base);
    let _ = writeln!(
        output,
        "Package:     {} {}",
        target.package.id, target.package.version
    );
    let _ = writeln!(output, "Digest:      {}", target.digest);
    let _ = writeln!(output, "State:       {}", state_label(target));
    match (&head.package, &head.digest) {
        (None, None) => {
            let _ = writeln!(output, "Current pin: none");
        }
        (package, digest) => {
            let _ = writeln!(
                output,
                "Current pin: {} {}",
                package
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), ToString::to_string),
                digest
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), ToString::to_string),
            );
        }
    }
    output
}

fn render_migration(result: &MigrationResult) -> String {
    let what = summary(&result.package, &result.version, &result.digest);
    match result.status {
        MigrationStatus::Migrated => format!(
            "Pinned profile {} to {what}.\nThe daemon reads the profile at the next launch.\n",
            result.name
        ),
        MigrationStatus::Unchanged => {
            format!(
                "Profile {} already pins {what}; nothing changed.\n",
                result.name
            )
        }
    }
}

/// Run one `pohunek plugin profile` subcommand.
///
/// # Errors
///
/// Returns [`CliError`] when the agents directory or
/// profile is unusable, the target does not resolve, consent is missing, the
/// rewrite fails, or the daemon fails.
pub(crate) async fn run(action: ProfileAction) -> Result<(), CliError> {
    match action {
        ProfileAction::List { json } => {
            let paths = Paths::resolve()?;
            let mut client = Client::connect(LOCAL_HOST, &paths).await?;
            let packages = list(&mut client).await?.packages;
            let dir = open_agents_dir(&paths.config_dir.join(AGENTS_DIR))
                .map_err(|error| directory_error(&error))?;
            let listing = list_profiles(dir.as_ref(), &packages)?;
            print_output(json, &listing, || render_listing(&listing))
        }
        ProfileAction::Migrate(args) => run_migrate(&args).await,
    }
}

async fn run_migrate(args: &MigrateArgs) -> Result<(), CliError> {
    let paths = Paths::resolve()?;
    let mut client = Client::connect(LOCAL_HOST, &paths).await?;
    let packages = list(&mut client).await?.packages;
    let name = args.name.as_str();
    let dir = open_agents_dir(&paths.config_dir.join(AGENTS_DIR))
        .map_err(|error| directory_error(&error))?
        .ok_or_else(|| Error::ProfileNotFound {
            name: name.to_owned(),
        })?;
    let loaded = load_profile(&dir, name).map_err(|fault| match fault {
        LoadFault::Missing => Error::ProfileNotFound {
            name: name.to_owned(),
        },
        other => Error::ProfileUnusable {
            name: name.to_owned(),
            detail: other.detail().to_owned(),
        },
    })?;
    let head = parse_head(&loaded.text).map_err(|detail| Error::ProfileUnusable {
        name: name.to_owned(),
        detail,
    })?;
    let target = resolve_target(name, &head.base, &packages, args.digest.as_ref())?;
    let result = |status| MigrationResult {
        name: name.to_owned(),
        base: head.base.clone(),
        package: target.package.id.clone(),
        version: target.package.version.clone(),
        digest: target.digest.clone(),
        previous_package: head.package.clone(),
        previous_digest: head.digest.clone(),
        status,
    };
    if already_pinned(&head, target) {
        let result = result(MigrationStatus::Unchanged);
        return print_output(args.json, &result, || render_migration(&result));
    }
    if !args.yes {
        if !args.json {
            print!("{}", render_review(name, &head, target));
        }
        return Err(Error::ConsentRequired {
            verb: "migrating",
            summary: format!(
                "profile {name} to {}",
                summary(&target.package.id, &target.package.version, &target.digest)
            ),
        }
        .into());
    }
    let pin = Pin {
        package: target.package.id.clone(),
        digest: target.digest.clone(),
    };
    write_pin(&dir, name, &loaded, &pin).map_err(|detail| Error::ProfileWrite {
        name: name.to_owned(),
        detail,
    })?;
    let result = result(MigrationStatus::Migrated);
    print_output(args.json, &result, || render_migration(&result))
}

fn temporary_name() -> Result<String, String> {
    let mut bytes = [0_u8; TEMPORARY_RANDOM_BYTES];
    getrandom::getrandom(&mut bytes)
        .map_err(|_error| "the system random source failed".to_owned())?;
    let mut name = String::from(TEMPORARY_PREFIX);
    for byte in bytes {
        let _ = write!(name, "{byte:02x}");
    }
    Ok(name)
}

/// Rewrite the profile atomically and verify the result.
///
/// The original is displaced under a private quarantine name and stays there
/// until the new file is verified, so a profile changed by anyone else since
/// it was read is detected rather than overwritten, and a failed verification
/// leaves the original recoverable.
fn write_pin(
    dir: &TrustedDir,
    name: &str,
    loaded: &LoadedProfile,
    pin: &Pin,
) -> Result<(), String> {
    let file = profile_file_name(name);
    let rewritten = apply_pin(&loaded.text, pin)?;
    let mode = loaded.mode & REWRITE_MODE_CEILING;
    let replaced = dir
        .replace_file_displacing(
            &file,
            temporary_name()?,
            rewritten.as_bytes(),
            mode,
            DestinationExpectation::Exact {
                identity: loaded.identity,
                mode: loaded.mode,
                content: loaded.text.as_bytes(),
            },
        )
        .map_err(|error| match error {
            DisplacingReplaceError::BeforeCommit(FsError::IdentityChanged { .. }) => {
                "the profile changed while it was being rewritten; nothing was changed, run the command again"
                    .to_owned()
            }
            DisplacingReplaceError::BeforeCommit(_) => {
                "the profile could not be replaced; it is unchanged".to_owned()
            }
            DisplacingReplaceError::CommittedDurabilityUncertain { .. } => {
                "the profile was rewritten but the directory could not be synchronized; run the command again to confirm"
                    .to_owned()
            }
            DisplacingReplaceError::RecoveryRequired { quarantine, .. } => format!(
                "the replacement failed and the original remains at {}",
                quarantine.display()
            ),
            _ => "the profile could not be replaced".to_owned(),
        })?;
    let quarantine = replaced.displaced.as_ref().map(StagedEntry::path);
    let verified = dir
        .read_file(&file, mode, MAX_PROFILE_BYTES)
        .ok()
        .filter(|bytes| bytes == rewritten.as_bytes())
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|text| parse_head(&text).ok())
        .is_some_and(|head| head.pin().as_ref() == Some(pin));
    if !verified {
        let kept = quarantine.map_or_else(String::new, |path| {
            format!("; the original is kept at {}", path.display())
        });
        return Err(format!("the rewritten profile failed verification{kept}"));
    }
    if let Some(original) = replaced.displaced {
        let at = original.path();
        original.remove().map_err(|_error| {
            format!(
                "the profile was rewritten but the previous copy remains at {}",
                at.display()
            )
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
