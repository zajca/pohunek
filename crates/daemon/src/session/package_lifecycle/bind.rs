//! `package.bind_profile`: pins a host agent profile to an installed package.
//!
//! The pin is written under the exclusive package lifecycle authority, the one
//! that serializes install, select, enable and uninstall. A concurrent
//! uninstall therefore computes its retained set either before the bind (and
//! the bind then fails because the target is gone) or after it (and the
//! uninstall is refused because the profile pins the target).

// Rust guideline compliant 2026-10-05

use package::registry::{RegistryState, RetainedDigests};
use package::PackageDigest;
use protocol::{
    PackageBindProfileParams, PackageBindProfileResult, PackageBindStatus, PackageErrorKind,
    PackageRuntimeInfo, RuntimeId,
};
use tracing::warn;

use super::selection::differs;
use super::{inspect_record, prove_loadable, read_state, Context, Inspected};
use crate::agent::host::ServedBy;
use std::sync::Arc;

use crate::agent::{apply_pin, parse_head, BindError, Pin, ProfileRegistry};
use crate::session::SessionRegistry;

impl SessionRegistry {
    /// Pins the host agent profile `params.profile` to an installed package.
    ///
    /// Without a digest the profile moves to the selected, enabled package
    /// that serves its base runtime. Only the `package` and `digest` keys of
    /// the profile change, and the new file replaces the old one atomically.
    /// A dry run reports the change without writing.
    ///
    /// # Errors
    ///
    /// Returns the [`PackageErrorKind`] naming the first failed check; on every
    /// error the profile is as it was found.
    pub async fn package_bind_profile(
        &self,
        params: PackageBindProfileParams,
    ) -> Result<PackageBindProfileResult, PackageErrorKind> {
        let profiles = self.inner.profiles.clone();
        self.package_transaction(move |context, retained| {
            bind_profile(context, &profiles, retained, &params)
        })
        .await
    }
}

/// The installed package a profile is pinned to.
struct Target {
    inspected: Inspected,
    runtime: PackageRuntimeInfo,
}

/// The error kind of a profile that could not be read or rewritten.
fn bind_kind(profile: &str, error: BindError) -> PackageErrorKind {
    match error {
        BindError::NotFound => PackageErrorKind::ProfileNotFound,
        BindError::Unusable(detail) => {
            warn!(%profile, %detail, "an agent profile cannot be rewritten");
            PackageErrorKind::ProfileUnusable
        }
        BindError::Changed => {
            warn!(%profile, "an agent profile changed while it was being rewritten");
            PackageErrorKind::ProfileChanged
        }
        BindError::Failed(reason) => {
            warn!(%profile, %reason, "an agent profile could not be rewritten");
            PackageErrorKind::RegistryFailed
        }
    }
}

fn unusable(profile: &str, detail: &str) -> PackageErrorKind {
    warn!(%profile, %detail, "an agent profile cannot be rewritten");
    PackageErrorKind::ProfileUnusable
}

/// Picks and proves the package `base` is pinned to.
///
/// With a digest, the installed package of that digest; without one, the one
/// selected and enabled package serving `base`. The package must load, serve
/// exactly `base` and pass the runtime-id claim rules.
fn resolve_target(
    context: &Context,
    base: &RuntimeId,
    digest: Option<&PackageDigest>,
    retained: &RetainedDigests,
) -> Result<Target, PackageErrorKind> {
    if matches!(context.runtimes.served_by(base), Some(ServedBy::Builtin)) {
        return Err(PackageErrorKind::ProfileBaseBuiltin);
    }
    let state = read_state(context)?;
    let report = context.runtimes.package_report();
    let mut serving: Vec<Inspected> = state
        .packages()
        .iter()
        .map(|record| inspect_record(context, &state, record, retained, &report))
        .filter(|inspected| inspected.info.runtime_id.as_ref() == Some(base))
        .collect();
    let chosen = if let Some(digest) = digest {
        let index = serving
            .iter()
            .position(|inspected| &inspected.info.digest == digest)
            .ok_or(PackageErrorKind::ProfileTargetInvalid)?;
        serving.swap_remove(index)
    } else {
        serving.retain(|inspected| inspected.info.selected && inspected.info.enabled);
        match serving.len() {
            1 => serving.swap_remove(0),
            _ => return Err(PackageErrorKind::ProfileTargetInvalid),
        }
    };
    if chosen.info.fault.is_some() {
        return Err(PackageErrorKind::ProfileTargetInvalid);
    }
    prove_loadable(context, &chosen.info.digest)?;
    let runtime = chosen
        .runtime
        .clone()
        .ok_or(PackageErrorKind::ProfileTargetInvalid)?;
    Ok(Target {
        inspected: chosen,
        runtime,
    })
}

/// Refuses a pin of a version whose integration differs from the selected
/// version of the same package.
///
/// A pin is a retained reference, so it must not make an unselected version
/// with another handler or hook schema coexist with the selected one, which is
/// the state the selection gate keeps from arising in the other order.
fn ensure_coexists_with_selected(
    context: &Context,
    state: &RegistryState,
    digest: &PackageDigest,
) -> Result<(), PackageErrorKind> {
    let record = state
        .package(digest)
        .ok_or(PackageErrorKind::NotInstalled)?;
    let Some(selected) = state
        .selected(&record.identity().id)
        .filter(|selected| *selected != digest)
        .and_then(|selected| state.package(selected))
    else {
        return Ok(());
    };
    if differs(context, record, selected) {
        warn!(
            package = %record.identity().id,
            "a profile cannot pin a version whose integration differs from the selected one"
        );
        return Err(PackageErrorKind::IntegrationIncompatible);
    }
    Ok(())
}

/// Reads, validates, edits and publishes the profile under the lifecycle
/// authority the caller holds.
fn bind_profile(
    context: &Context,
    profiles: &ProfileRegistry,
    retained: &RetainedDigests,
    params: &PackageBindProfileParams,
) -> Result<PackageBindProfileResult, PackageErrorKind> {
    let name = params.profile.as_str();
    let dir = profiles
        .open_for_bind()
        .map_err(|error| bind_kind(name, error))?;
    if !params.dry_run {
        dir.remove_stale_temporaries();
    }
    let file = dir.read(name).map_err(|error| bind_kind(name, error))?;
    let head = parse_head(file.text()).map_err(|detail| unusable(name, &detail))?;
    let target = resolve_target(context, &head.base, params.digest.as_ref(), retained)?;
    let pin = Pin {
        package: target.inspected.info.package.id.clone(),
        digest: target.inspected.info.digest.clone(),
    };
    let result = |status, reloaded, referenced| {
        let mut package = target.inspected.info.clone();
        package.referenced = referenced;
        PackageBindProfileResult {
            status,
            profile: name.to_owned(),
            base: head.base.clone(),
            previous: head.digest.clone(),
            package,
            runtime: target.runtime.clone(),
            reloaded,
        }
    };
    if head.pin().as_ref() == Some(&pin) {
        return Ok(result(PackageBindStatus::Unchanged, false, true));
    }
    ensure_coexists_with_selected(context, &read_state(context)?, &pin.digest)?;
    let rewritten = apply_pin(file.text(), &pin).map_err(|detail| unusable(name, &detail))?;
    // The candidate must resolve against the target exactly as a launch of it
    // would (size bound, structure, resume and fork overrides against the
    // target's capabilities), in a preview as in a commit, so a published
    // profile is never one the launch or retention path rejects.
    let definition = context
        .store
        .read_definition(&pin.digest, &target.inspected.info.package)
        .map_err(|_rejection| PackageErrorKind::ProfileTargetInvalid)?;
    profiles
        .validate_candidate(name, &rewritten, &Arc::new(definition))
        .map_err(|error| unusable(name, &error.msg))?;
    if params.dry_run {
        return Ok(result(
            PackageBindStatus::Preview,
            false,
            target.inspected.info.referenced,
        ));
    }
    dir.publish(name, &file, &rewritten)
        .map_err(|error| bind_kind(name, error))?;
    Ok(result(PackageBindStatus::Bound, true, true))
}
