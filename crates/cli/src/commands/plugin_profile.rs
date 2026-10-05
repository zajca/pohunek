//! `pohunek plugin profile`: pin host agent profiles to installed packages.
//!
//! A host agent profile (`<config_dir>/agents/<name>.toml`) extends a base
//! runtime. When an installed package serves that runtime, the profile must
//! carry an explicit `package` and `digest` pin. A profile moves to another
//! digest only through `migrate`, which asks the daemon to rewrite exactly
//! those two keys under its package lifecycle authority; this command never
//! writes the file itself. `list` is informational: it reads the files with its
//! own reader, and the daemon decides what a profile resolves to.
//!
//! Profile files can hold secret `[env]` values. Nothing here deserializes
//! `[env]`, prints file content, or echoes a parse error's source line: a
//! diagnostic names a key or a line number only.

// Rust guideline compliant 2026-10-05

use std::fmt::Write as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use clap::{Args, Subcommand};
use pohunek_platform::filesystem::{EntryKind, FsError, TrustedDir};
use protocol::{
    method, PackageBindProfileParams, PackageBindProfileResult, PackageBindStatus, PackageDigest,
    PackageId, PackageInfo, RuntimeId,
};
use serde::Serialize;
use toml_edit::{DocumentMut, TomlError};

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

/// Permission bits that let another account modify a file.
const OTHERS_WRITE_BITS: u32 = 0o022;

/// Permission bits of a file mode.
const MODE_BITS: u32 = 0o7777;

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

    /// Pin a profile to an installed package; the daemon rewrites only its
    /// `package` and `digest` keys.
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

/// The keys of a profile the commands read.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProfileHead {
    base: RuntimeId,
    package: Option<PackageId>,
    digest: Option<PackageDigest>,
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
    dir.entry_identity_with_mode(&file, EntryKind::RegularFile, mode)
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
    Ok(LoadedProfile { text })
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

/// Review of the change a preview announced.
fn render_review(result: &PackageBindProfileResult) -> String {
    let mut output = String::from("The profile would change:\n");
    let _ = writeln!(output, "Profile:     {}", result.profile);
    let _ = writeln!(output, "Base:        {}", result.base);
    let _ = writeln!(
        output,
        "Package:     {} {}",
        result.package.package.id, result.package.package.version
    );
    let _ = writeln!(output, "Digest:      {}", result.package.digest);
    let _ = writeln!(output, "State:       {}", state_label(&result.package));
    match &result.previous {
        None => {
            let _ = writeln!(output, "Current pin: none");
        }
        Some(digest) => {
            let _ = writeln!(output, "Current pin: {digest}");
        }
    }
    output
}

fn render_migration(result: &PackageBindProfileResult) -> String {
    let what = summary(
        &result.package.package.id,
        &result.package.package.version,
        &result.package.digest,
    );
    match result.status {
        PackageBindStatus::Unchanged => {
            format!(
                "Profile {} already pins {what}; nothing changed.\n",
                result.profile
            )
        }
        PackageBindStatus::Bound | PackageBindStatus::Preview => format!(
            "Pinned profile {} to {what}.\nThe daemon reads the profile at the next launch.\n",
            result.profile
        ),
    }
}

/// Run one `pohunek plugin profile` subcommand.
///
/// # Errors
///
/// Returns [`CliError`] when the agents directory is unusable (`list`), a
/// `--digest` prefix does not name one package, consent is missing, or the
/// daemon refuses or fails the request.
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
    let name = args.name.as_str();
    let requested = match &args.digest {
        None => None,
        Some(DigestSelector::Full(digest)) => Some(digest.clone()),
        Some(selector @ DigestSelector::Prefix(_)) => {
            let packages = list(&mut client).await?.packages;
            Some(resolve_prefix(name, selector, &packages)?)
        }
    };
    let preview = client
        .call::<method::PackageBindProfile>(PackageBindProfileParams {
            profile: name.to_owned(),
            digest: requested,
            dry_run: true,
        })
        .await?;
    if preview.status == PackageBindStatus::Unchanged {
        return print_output(args.json, &preview, || render_migration(&preview));
    }
    if !args.yes {
        if !args.json {
            print!("{}", render_review(&preview));
        }
        return Err(Error::ConsentRequired {
            verb: "migrating",
            summary: format!(
                "profile {name} to {}",
                summary(
                    &preview.package.package.id,
                    &preview.package.package.version,
                    &preview.package.digest
                )
            ),
        }
        .into());
    }
    // The call names the package the owner consented to, so a package
    // selected between the preview and this call cannot change the target.
    let bound = client
        .call::<method::PackageBindProfile>(PackageBindProfileParams {
            profile: name.to_owned(),
            digest: Some(preview.package.digest.clone()),
            dry_run: false,
        })
        .await?;
    print_output(args.json, &bound, || render_migration(&bound))
}

/// The one installed digest a hex prefix names.
fn resolve_prefix(
    name: &str,
    selector: &DigestSelector,
    packages: &[PackageInfo],
) -> Result<PackageDigest, Error> {
    let target = |detail: String| Error::ProfileTarget {
        name: name.to_owned(),
        detail,
    };
    let matching: Vec<&PackageInfo> = packages
        .iter()
        .filter(|info| selector.matches(&info.digest))
        .collect();
    match matching.as_slice() {
        [] => Err(target("no installed package has that digest".to_owned())),
        [only] => Ok(only.digest.clone()),
        several => {
            let listed: Vec<String> = several
                .iter()
                .map(|info| summary(&info.package.id, &info.package.version, &info.digest))
                .collect();
            Err(target(format!(
                "several packages match: {}; pass a longer --digest",
                listed.join(", ")
            )))
        }
    }
}

#[cfg(test)]
mod tests;
