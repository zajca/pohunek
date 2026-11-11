//! `package.create_profile`: writes a new host agent profile pinned to the
//! selected package of its base runtime.
//!
//! The daemon composes the text, validates it through the resolution a launch
//! uses, and publishes it under the exclusive package lifecycle authority that
//! `package.bind_profile` uses. An uninstall of the target therefore either
//! runs before the create (which then fails because the target is gone) or
//! after it (and is refused because the new profile pins the target).
//!
//! The only `[env]` entry a composed profile can hold is the base runtime's
//! config home variable, taken from its descriptor. Arguments and the home path
//! pass the credential check before anything is written, and no diagnostic
//! carries a request value.

// Rust guideline compliant 2026-11-11

use std::sync::Arc;

use protocol::{
    PackageCreateProfileParams, PackageCreateProfileResult, PackageCreateProfileStatus,
    PackageErrorKind, ProfileDigest,
};
use sha2::{Digest as _, Sha256};
use tracing::warn;

use super::bind::{bind_kind, ensure_coexists, resolve_target, unusable, Target};
use super::{read_state, Context, PackageService, RetainedPins};
use crate::agent::host::ConfigHome;
use crate::agent::{
    compose_profile, credential_arg_problem, credential_literal_problem, BindError, Pin,
    ProfileRegistry, ProfileSpec,
};
use crate::project::config::validate_name;

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
    let home = params
        .home
        .as_deref()
        .map(|home| {
            home_entry(
                definition
                    .as_ref()
                    .and_then(|definition| definition.config_home()),
                home,
            )
        })
        .transpose()?;
    if params
        .home
        .as_deref()
        .is_some_and(|home| credential_literal_problem(home).is_some())
    {
        warn!(%name, "the home path of a new profile looks like a credential");
        return Err(PackageErrorKind::ProfileCredentialLiteral);
    }
    if let Some(index) = credential_arg_problem(&params.args) {
        warn!(%name, index, "an argument of a new profile looks like a credential");
        return Err(PackageErrorKind::ProfileCredentialLiteral);
    }
    let pin = target.as_ref().map(pin_of);
    let spec = ProfileSpec {
        base: params.base.clone(),
        pin: pin.clone(),
        home,
        args: params.args.clone(),
        secret_env: params.secret_env.clone(),
    };
    let text = compose_profile(&spec);
    if let Some(pin) = &pin {
        ensure_coexists(context, &read_state(context)?, retained.gate(), &pin.digest)?;
    }
    let validated = match &definition {
        Some(definition) => profiles.validate_candidate(name, &text, definition),
        None => profiles.validate_builtin_candidate(name, &text),
    };
    validated.map_err(|error| unusable(name, &error.msg))?;
    let existing = match &dir {
        Some(dir) => dir
            .read_existing(name)
            .map_err(|error| bind_kind(name, error))?,
        None => None,
    };
    let previous = match (&existing, &params.replace) {
        (Some(_), None) => return Err(PackageErrorKind::ProfileExists),
        (Some(file), Some(expected)) => {
            let current = file_digest(file.text().as_bytes());
            if &current != expected {
                return Err(PackageErrorKind::ProfileDigestMismatch);
            }
            Some(current)
        }
        (None, Some(_)) => return Err(PackageErrorKind::ProfileNotFound),
        (None, None) => None,
    };
    let status = if params.dry_run {
        PackageCreateProfileStatus::Preview
    } else if existing.is_some() {
        PackageCreateProfileStatus::Replaced
    } else {
        PackageCreateProfileStatus::Created
    };
    if let Some(dir) = dir.as_ref().filter(|_dir| !params.dry_run) {
        dir.remove_stale_temporaries();
        match &existing {
            Some(file) => dir
                .publish(name, file, &text)
                .map_err(|error| match error {
                    BindError::Changed => PackageErrorKind::ProfileDigestMismatch,
                    other => bind_kind(name, other),
                })?,
            None => dir
                .create_new(name, &text)
                .map_err(|error| bind_kind(name, error))?,
        }
    } else if !params.dry_run {
        // The directory was created by `open_for_create(true)`; a missing one
        // here would be a logic error, so fail closed.
        return Err(PackageErrorKind::RegistryFailed);
    }
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

fn pin_of(target: &Target) -> Pin {
    Pin {
        package: target.inspected.info.package.id.clone(),
        digest: target.inspected.info.digest.clone(),
    }
}
