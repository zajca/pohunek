//! `package.remove_profile`: remove a host agent profile or preset file that
//! is still exactly the version the caller names.
//!
//! The removal runs under the exclusive package lifecycle authority, so no
//! resume resolves the profile and no create commits a session of it while
//! the checks and the unlink run. A name without a file that resolves to a
//! runtime (the built-in `shell` or a runtime an installed package serves) is
//! refused by kind, as is a name that opens a profile stored under another
//! spelling on a case-insensitive volume. A file whose digest differs is left
//! as found, and a profile that a session which is not terminal runs, or that
//! a preset names as its parent, is refused; a session or preset holds the
//! profile when its name opens the same inode now, whether through the same
//! name, another spelling or an in-tree symbolic link. The file is unlinked
//! through its trusted directory descriptor and bound to the inode that was
//! read, so a symbolic link or a file swapped in after the read is never
//! removed.

// Rust guideline compliant 2026-11-14

use std::collections::BTreeSet;

use protocol::{
    PackageErrorKind, PackageRemoveProfileParams, PackageRemoveProfileResult, RuntimeId,
};
use tracing::warn;

use super::bind::bind_kind;
use super::create::file_digest;
use super::PackageService;
use crate::agent::host::ServedBy;
use crate::agent::{BindError, ProfileRegistry};
use crate::project::config::validate_name;

impl PackageService {
    /// Removes the host agent profile `params.profile` when its file's digest
    /// is `params.digest`.
    ///
    /// # Errors
    ///
    /// Returns the [`PackageErrorKind`] naming the first failed check. On
    /// every error but [`PackageErrorKind::RegistryFailed`] the profile file
    /// was left as found; that one follows a failed move, unlink or directory
    /// synchronization, and the daemon log says where the file is.
    pub async fn package_remove_profile(
        &self,
        params: PackageRemoveProfileParams,
    ) -> Result<PackageRemoveProfileResult, PackageErrorKind> {
        let profiles = self.sessions.profiles().clone();
        self.profile_transaction(move |live| remove_profile(&profiles, live, &params))
            .await
    }
}

/// Checks and removes the profile under the lifecycle authority the caller
/// holds; `live` holds the agents of the sessions that are not terminal.
fn remove_profile(
    profiles: &ProfileRegistry,
    live: &BTreeSet<String>,
    params: &PackageRemoveProfileParams,
) -> Result<PackageRemoveProfileResult, PackageErrorKind> {
    let name = params.profile.as_str();
    if validate_name("agent", name).is_err() {
        return Err(PackageErrorKind::ProfileNotFound);
    }
    let dir = match profiles.open_for_bind() {
        Ok(dir) => dir,
        Err(BindError::NotFound) => return Err(no_profile_file(profiles, name)),
        Err(error) => return Err(bind_kind(name, error)),
    };
    let file = match dir.read(name) {
        Ok(file) => file,
        Err(BindError::NotFound) => return Err(no_profile_file(profiles, name)),
        Err(error) => return Err(bind_kind(name, error)),
    };
    if !dir
        .is_stored_name(name)
        .map_err(|error| bind_kind(name, error))?
    {
        warn!(%name, "the name opens a profile stored under another spelling, so nothing is removed");
        return Err(PackageErrorKind::ProfileNotFound);
    }
    let current = file_digest(file.text().as_bytes());
    if current != params.digest {
        return Err(PackageErrorKind::ProfileDigestMismatch);
    }
    // A session or a preset holds the profile when its name opens this file
    // now: the same name, or another spelling or an in-tree link that a
    // launch follows to the same inode.
    let target = file.file_id();
    let opens_target = |other: &str| other == name || dir.launch_file_id(other) == Some(target);
    if let Some(agent) = live.iter().find(|agent| opens_target(agent)) {
        warn!(%name, %agent, "a running session uses the agent profile, so it is not removed");
        return Err(PackageErrorKind::ProfileInUse);
    }
    let presets = profiles.presets_of(opens_target).map_err(|error| {
        warn!(%name, %error, "the agents directory cannot be scanned for presets of the profile");
        PackageErrorKind::RegistryFailed
    })?;
    if !presets.is_empty() {
        warn!(%name, ?presets, "presets name the agent profile as their parent, so it is not removed");
        return Err(PackageErrorKind::ProfileHasPresets);
    }
    dir.remove_stale_temporaries();
    dir.remove(name, &file).map_err(|error| match error {
        BindError::Changed => {
            warn!(%name, "the agent profile changed before its removal and was left as found");
            PackageErrorKind::ProfileDigestMismatch
        }
        other => bind_kind(name, other),
    })?;
    Ok(PackageRemoveProfileResult {
        profile: name.to_owned(),
        file_digest: current,
    })
}

/// Why `name`, which has no profile file, cannot be removed: it is a runtime
/// the host serves, or there is no such profile.
fn no_profile_file(profiles: &ProfileRegistry, name: &str) -> PackageErrorKind {
    match RuntimeId::parse(name)
        .ok()
        .and_then(|runtime| profiles.runtimes().served_by(&runtime))
    {
        Some(ServedBy::Builtin) => PackageErrorKind::ProfileBuiltin,
        Some(ServedBy::Package(_)) => PackageErrorKind::ProfilePackageRuntime,
        None => PackageErrorKind::ProfileNotFound,
    }
}
