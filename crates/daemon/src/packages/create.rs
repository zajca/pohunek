//! `package.create_profile` and `package.create_preset`: write a new host
//! agent profile pinned to the selected package of its base runtime, or a
//! preset of an existing profile.
//!
//! The daemon composes the text, validates it through the resolution a launch
//! uses, and publishes it under the exclusive package lifecycle authority that
//! `package.bind_profile` uses. An uninstall of the target therefore either
//! runs before the create (which then fails because the target is gone) or
//! after it (and is refused because the new profile pins the target).
//!
//! The only `[env]` entry a composed profile can hold is the base runtime's
//! config home variable, taken from its descriptor. Arguments and the home path
//! pass the credential check before anything is written, the `[options]`
//! tables pass the load rules of a launch, and no diagnostic carries a request
//! value. A preset holds only its parent and choices and is validated through
//! the preset resolution of a launch.

// Rust guideline compliant 2026-11-14

use std::sync::Arc;

use protocol::{
    PackageCreatePresetParams, PackageCreateProfileParams, PackageCreateProfileResult,
    PackageCreateProfileStatus, PackageErrorKind, ProfileDigest, MAX_PROFILE_OPTIONS,
};
use sha2::{Digest as _, Sha256};
use toml_edit::{DocumentMut, Table};
use tracing::warn;

use super::bind::{bind_kind, ensure_coexists, resolve_target, unusable, Target};
use super::{read_state, Context, PackageService, RetainedPins};
use crate::agent::host::{ConfigHome, RuntimeDefinition};
use crate::agent::{
    compose_preset, compose_profile, credential_arg_problem, credential_literal_problem,
    env_name_problem, BindDir, BindError, Pin, PresetSpec, ProfileFile, ProfileRegistry,
    ProfileSpec, MAX_PROFILE_BYTES,
};
use crate::project::config::validate_name;

/// The table of a profile file that holds its session rights.
const RIGHTS_KEY: &str = "rights";

/// Root key of the options text `package.create_profile` accepts.
const OPTIONS_KEY: &str = "options";

/// Longest accepted home path in bytes; the usual `PATH_MAX`.
const HOME_MAX_BYTES: usize = 4096;

impl PackageService {
    /// Creates the host agent profile `params.profile`.
    ///
    /// A package-served base is pinned to the one selected, enabled package
    /// that serves it. An existing name is replaced only when `params.replace`
    /// is the digest of its current file. A dry run reports the text and writes
    /// nothing, not even the agents directory.
    ///
    /// # Errors
    ///
    /// Returns the [`PackageErrorKind`] naming the first failed check; on every
    /// error no profile file was written or changed.
    pub async fn package_create_profile(
        &self,
        params: PackageCreateProfileParams,
    ) -> Result<PackageCreateProfileResult, PackageErrorKind> {
        let profiles = self.sessions.profiles().clone();
        // A replaced profile's old pin is not a reference to compare the new
        // pin with.
        let replaced = params.replace.is_some().then(|| params.profile.clone());
        self.package_transaction_excluding(replaced, move |context, retained| {
            create_profile(context, &profiles, retained, &params)
        })
        .await
    }

    /// Creates the host agent preset `params.profile` of `params.parent`.
    ///
    /// The preset text holds only the parent, the choices and `locked`, and
    /// is validated through the preset resolution of a launch. An existing
    /// name is replaced only when `params.replace` is the digest of its
    /// current file. A dry run reports the text and writes nothing.
    ///
    /// # Errors
    ///
    /// Returns the [`PackageErrorKind`] naming the first failed check; on every
    /// error no profile file was written or changed.
    pub async fn package_create_preset(
        &self,
        params: PackageCreatePresetParams,
    ) -> Result<PackageCreateProfileResult, PackageErrorKind> {
        let profiles = self.sessions.profiles().clone();
        let replaced = params.replace.is_some().then(|| params.profile.clone());
        self.package_transaction_excluding(replaced, move |_context, _retained| {
            create_preset(&profiles, &params)
        })
        .await
    }
}

/// The digest of `bytes` as a profile file digest.
pub(crate) fn file_digest(bytes: &[u8]) -> ProfileDigest {
    let hash = Sha256::digest(bytes);
    ProfileDigest::parse(&format!("sha256:{hash:x}"))
        .unwrap_or_else(|_invalid| unreachable!("a SHA-256 digest is 64 lowercase hex digits"))
}

/// Whether `home` is an absolute path of plain components.
///
/// Refused: a relative path, a `~` tilde prefix on any component, `.` and `..`
/// components, empty components, control characters and an over-long path.
fn home_is_plain(home: &str) -> bool {
    let Some(relative) = home.strip_prefix('/') else {
        return false;
    };
    home.len() <= HOME_MAX_BYTES
        && !home.chars().any(char::is_control)
        && relative
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".." && !part.starts_with('~'))
}

/// The config home variable and value to write, or the refusal.
fn home_entry(
    config_home: Option<&ConfigHome>,
    home: &str,
) -> Result<(String, String), PackageErrorKind> {
    let declared = config_home.ok_or(PackageErrorKind::ProfileHomeUnsupported)?;
    if declared.subdir().is_some() || !home_is_plain(home) {
        return Err(PackageErrorKind::ProfileHomeInvalid);
    }
    Ok((declared.env().to_owned(), home.to_owned()))
}

/// Validates, composes and publishes the profile under the lifecycle authority
/// the caller holds.
fn create_profile(
    context: &Context,
    profiles: &ProfileRegistry,
    retained: &RetainedPins,
    params: &PackageCreateProfileParams,
) -> Result<PackageCreateProfileResult, PackageErrorKind> {
    let name = params.profile.as_str();
    if let Err(error) = validate_name("agent", name) {
        return Err(unusable(name, &error.msg));
    }
    let dir = profiles
        .open_for_create(!params.dry_run)
        .map_err(|error| bind_kind(name, error))?;
    let target = match resolve_target(context, &params.base, None, retained) {
        Ok(target) => Some(target),
        Err(PackageErrorKind::ProfileBaseBuiltin) => None,
        Err(kind) => return Err(kind),
    };
    let definition = match &target {
        Some(target) => Some(Arc::new(
            context
                .store
                .read_definition(
                    &target.inspected.info.digest,
                    &target.inspected.info.package,
                )
                .map_err(|_rejection| PackageErrorKind::ProfileTargetInvalid)?,
        )),
        None => None,
    };
    let home = checked_home(params, definition.as_deref(), name)?;
    let options = params
        .options_toml
        .as_deref()
        .map(|text| options_table(name, text))
        .transpose()?;
    let pin = target.as_ref().map(pin_of);
    let spec = ProfileSpec {
        base: params.base.clone(),
        pin: pin.clone(),
        home,
        args: params.args.clone(),
        secret_env: params.secret_env.clone(),
        options,
    };
    let text = carry_rights(dir.as_ref(), name, compose_profile(&spec))?;
    if let Some(pin) = &pin {
        ensure_coexists(context, &read_state(context)?, retained.gate(), &pin.digest)?;
    }
    let validated = match &definition {
        Some(definition) => profiles.validate_candidate(name, &text, definition),
        None => profiles.validate_builtin_candidate(name, &text),
    };
    validated.map_err(|error| unusable(name, &error.msg))?;
    let (status, previous) = write_file(
        dir.as_ref(),
        name,
        params.replace.as_ref(),
        params.dry_run,
        &text,
    )?;
    let home_env = spec.home.map(|(env, _path)| env);
    let (package, runtime) = match target {
        Some(Target { inspected, runtime }) => {
            let mut package = inspected.info;
            if !params.dry_run {
                package.referenced = true;
            }
            (Some(package), Some(runtime))
        }
        None => (None, None),
    };
    Ok(PackageCreateProfileResult {
        status,
        profile: name.to_owned(),
        base: params.base.clone(),
        package,
        runtime,
        home_env,
        file_digest: file_digest(text.as_bytes()),
        text,
        previous,
    })
}

/// Validates, composes and publishes the preset under the lifecycle
/// authority the caller holds.
fn create_preset(
    profiles: &ProfileRegistry,
    params: &PackageCreatePresetParams,
) -> Result<PackageCreateProfileResult, PackageErrorKind> {
    let name = params.profile.as_str();
    if let Err(error) = validate_name("agent", name) {
        return Err(unusable(name, &error.msg));
    }
    // The file the preset would replace is the parent it names.
    if params.parent == params.profile {
        return Err(unusable(name, "a preset cannot be its own parent"));
    }
    if params.options.len() > MAX_PROFILE_OPTIONS {
        return Err(unusable(
            name,
            "the preset selects more options than a profile declares",
        ));
    }
    let dir = profiles
        .open_for_create(!params.dry_run)
        .map_err(|error| bind_kind(name, error))?;
    refuse_preset_over_rights(dir.as_ref(), name)?;
    let text = compose_preset(&PresetSpec {
        parent: &params.parent,
        options: &params.options,
        locked: params.locked,
    });
    let agent = profiles
        .resolve_preset_candidate(name, &text)
        .map_err(|error| unusable(name, &error.msg))?;
    let (status, previous) = write_file(
        dir.as_ref(),
        name,
        params.replace.as_ref(),
        params.dry_run,
        &text,
    )?;
    Ok(PackageCreateProfileResult {
        status,
        profile: name.to_owned(),
        base: agent.base,
        package: None,
        runtime: None,
        home_env: None,
        file_digest: file_digest(text.as_bytes()),
        text,
        previous,
    })
}

/// The `[rights]` table of the profile file `name` holds, if it has one.
fn existing_rights(
    dir: Option<&BindDir>,
    name: &str,
) -> Result<Option<toml_edit::Item>, PackageErrorKind> {
    let Some(dir) = dir else {
        return Ok(None);
    };
    let Some(file) = dir
        .read_existing(name)
        .map_err(|error| bind_kind(name, error))?
    else {
        return Ok(None);
    };
    // A file that does not parse is being repaired, so it carries nothing.
    Ok(file
        .text()
        .parse::<DocumentMut>()
        .ok()
        .and_then(|document| document.get(RIGHTS_KEY).cloned()))
}

/// `text` with the `[rights]` table of the profile file it replaces.
///
/// A profile definition carries no rights of its own, so a replacement never
/// widens what the owner wrote for the profile it replaces.
fn carry_rights(
    dir: Option<&BindDir>,
    name: &str,
    text: String,
) -> Result<String, PackageErrorKind> {
    let Some(rights) = existing_rights(dir, name)? else {
        return Ok(text);
    };
    let mut document: DocumentMut = text
        .parse()
        .map_err(|_error| unusable(name, "the composed profile is not valid TOML"))?;
    document.insert(RIGHTS_KEY, rights);
    Ok(document.to_string())
}

/// Refuses a preset over a profile file that holds a `[rights]` table: a
/// preset takes its parent's rights and could not keep that table.
fn refuse_preset_over_rights(dir: Option<&BindDir>, name: &str) -> Result<(), PackageErrorKind> {
    if existing_rights(dir, name)?.is_some() {
        return Err(unusable(
            name,
            "the profile holds a [rights] table, which a preset cannot carry",
        ));
    }
    Ok(())
}

/// The `[options]` table of the options text `text` for profile `name`.
///
/// The text must be TOML whose only root key is `options`, a table; its
/// tables are validated later with the load rules of the composed profile.
/// Diagnostics never quote the text.
fn options_table(name: &str, text: &str) -> Result<Table, PackageErrorKind> {
    if u64::try_from(text.len()).map_or(true, |length| length > MAX_PROFILE_BYTES) {
        return Err(unusable(name, "the options text is too large"));
    }
    let mut document: DocumentMut = text
        .parse()
        .map_err(|_error| unusable(name, "the options text is not valid TOML"))?;
    let root = document.as_table_mut();
    let only_options = root.len() == 1 && root.contains_key(OPTIONS_KEY);
    match root.remove(OPTIONS_KEY) {
        Some(toml_edit::Item::Table(options)) if only_options => Ok(options),
        _ => Err(unusable(
            name,
            "the options file holds only [options] tables",
        )),
    }
}

/// Writes `text` as the profile `name` in `dir`, or replaces the existing
/// file whose digest is `replace`, and reports what the call did and the
/// replaced digest. A dry run applies the existence rules and writes
/// nothing.
fn write_file(
    dir: Option<&BindDir>,
    name: &str,
    replace: Option<&ProfileDigest>,
    dry_run: bool,
    text: &str,
) -> Result<(PackageCreateProfileStatus, Option<ProfileDigest>), PackageErrorKind> {
    let existing = match dir {
        Some(dir) => dir
            .read_existing(name)
            .map_err(|error| bind_kind(name, error))?,
        None => None,
    };
    let previous = previous_digest(existing.as_ref(), replace)?;
    let status = if dry_run {
        PackageCreateProfileStatus::Preview
    } else if existing.is_some() {
        PackageCreateProfileStatus::Replaced
    } else {
        PackageCreateProfileStatus::Created
    };
    if !dry_run {
        let dir = dir.ok_or(PackageErrorKind::RegistryFailed)?;
        publish(dir, name, existing.as_ref(), text)?;
    }
    Ok((status, previous))
}

fn pin_of(target: &Target) -> Pin {
    Pin {
        package: target.inspected.info.package.id.clone(),
        digest: target.inspected.info.digest.clone(),
    }
}

/// The home entry to write, after the home and credential checks.
fn checked_home(
    params: &PackageCreateProfileParams,
    definition: Option<&RuntimeDefinition>,
    name: &str,
) -> Result<Option<(String, String)>, PackageErrorKind> {
    let home = params
        .home
        .as_deref()
        .map(|home| home_entry(definition.and_then(RuntimeDefinition::config_home), home))
        .transpose()?;
    if params
        .home
        .as_deref()
        .is_some_and(|home| credential_literal_problem(home).is_some())
    {
        warn!(%name, "the home path of a new profile looks like a credential");
        return Err(PackageErrorKind::ProfileCredentialLiteral);
    }
    for (index, slot) in params.secret_env.iter().enumerate() {
        if credential_literal_problem(slot).is_some() {
            warn!(%name, index, "a secret_env slot of a new profile looks like a credential");
            return Err(PackageErrorKind::ProfileCredentialLiteral);
        }
        // A slot that is not a plain variable name is refused here, so the
        // launch rules never echo it.
        if env_name_problem(slot).is_some() {
            warn!(%name, index, "a secret_env slot of a new profile is not a variable name");
            return Err(PackageErrorKind::ProfileUnusable);
        }
    }
    if let Some(index) = credential_arg_problem(&params.args) {
        warn!(%name, index, "an argument of a new profile looks like a credential");
        return Err(PackageErrorKind::ProfileCredentialLiteral);
    }
    Ok(home)
}

/// The digest of the file a replace targets, after the existence rules: a name
/// that exists needs its digest, and a digest needs a name that exists.
fn previous_digest(
    existing: Option<&ProfileFile>,
    replace: Option<&ProfileDigest>,
) -> Result<Option<ProfileDigest>, PackageErrorKind> {
    match (existing, replace) {
        (Some(_), None) => Err(PackageErrorKind::ProfileExists),
        (Some(file), Some(expected)) => {
            let current = file_digest(file.text().as_bytes());
            if &current == expected {
                Ok(Some(current))
            } else {
                Err(PackageErrorKind::ProfileDigestMismatch)
            }
        }
        (None, Some(_)) => Err(PackageErrorKind::ProfileNotFound),
        (None, None) => Ok(None),
    }
}

/// Writes the new file, or replaces `existing` with `text`.
fn publish(
    dir: &BindDir,
    name: &str,
    existing: Option<&ProfileFile>,
    text: &str,
) -> Result<(), PackageErrorKind> {
    dir.remove_stale_temporaries();
    match existing {
        Some(file) => dir.publish(name, file, text).map_err(|error| match error {
            BindError::Changed => PackageErrorKind::ProfileDigestMismatch,
            other => bind_kind(name, other),
        }),
        None => dir
            .create_new(name, text)
            .map_err(|error| bind_kind(name, error)),
    }
}
