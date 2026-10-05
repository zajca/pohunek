//! Compatibility-gated package selection.
//!
//! An integration handler owns one active upstream asset set per runtime, so
//! two versions of a package that name different integrations cannot be served
//! at once: sessions launched from the old version keep reporting through its
//! handler and hook schema until they end. A version whose integration differs
//! from a version that something still references therefore stays installed
//! but is never selected until the last such reference is gone.
//!
//! The integration of a package is the pair of its handler id and hook schema
//! id, or none. Two packages are compatible when their integrations are equal:
//! every compiled handler drives exactly one schema, and the registry declares
//! no coexistence between different pairs, so any difference (including a
//! missing integration on one side) is incompatible. A retained version whose
//! integration cannot be determined is incompatible as well.
//!
//! The verdict is derived from the registry record and the retained set of the
//! moment; nothing is persisted, so a restart recomputes it. It is only
//! authoritative under the exclusive package lifecycle authority, which keeps
//! a fresh launch from pinning a digest between the verdict and the registry
//! write; reads (`package.list`, `package.inspect`, `package.doctor`) report
//! the verdict without that guarantee.

// Rust guideline compliant 2026-10-05

use std::collections::BTreeMap;

use package::registry::{PackageRecord, RegistryState, RetainedDigests};
use package::PackageDigest;
use protocol::{PackageId, PackageSelectionBlock, PackageSelectionBlockReason};

use super::Context;
use crate::agent::host::{PackageRejection, RuntimeDefinition};
use crate::integration::RetainedSchemas;

/// The integration a package declares: its handler id and hook schema id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Declared {
    handler: String,
    schema: &'static pohunek_worker_protocol::HookSchema,
}

/// What the integration of a package resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Integration {
    /// The package declares this integration, or none.
    Known(Option<Declared>),
    /// The descriptor cannot be read, so nothing is known about the package.
    Unresolvable,
}

/// The integration `definition` declares.
pub(super) fn declared_by(definition: &RuntimeDefinition) -> Integration {
    Integration::Known(
        definition
            .integration_handler()
            .zip(definition.hook_schema())
            .map(|(handler, schema)| Declared {
                handler: handler.as_str().to_owned(),
                schema,
            }),
    )
}

/// Memoized integrations of installed packages.
///
/// Reading one verifies the whole package root, so a request that compares a
/// candidate with several retained versions reads each of them once. Package
/// roots are content addressed, so a cached entry stays valid for the life of
/// the request.
#[derive(Debug, Default)]
pub(super) struct Integrations {
    resolved: BTreeMap<PackageDigest, Integration>,
}

impl Integrations {
    /// The integration of the installed package `record`.
    ///
    /// A descriptor that no longer parses as a definition because it predates
    /// `[integration] hook_schema` resolves through the handler it names, the
    /// same way a live worker launched from it does; anything else that cannot
    /// be read is [`Integration::Unresolvable`].
    pub(super) fn of(&mut self, context: &Context, record: &PackageRecord) -> Integration {
        self.resolved
            .entry(record.digest().clone())
            .or_insert_with(|| resolve(context, record))
            .clone()
    }
}

fn resolve(context: &Context, record: &PackageRecord) -> Integration {
    match context
        .store
        .read_definition(record.digest(), record.identity())
    {
        Ok(definition) => declared_by(&definition),
        Err(PackageRejection::Descriptor(_)) => legacy(context, record),
        Err(_unreadable) => Integration::Unresolvable,
    }
}

fn legacy(context: &Context, record: &PackageRecord) -> Integration {
    let handler = context
        .store
        .read_legacy_integration_handler(record.digest(), record.identity())
        .ok()
        .flatten();
    match handler.and_then(|handler| {
        pohunek_worker_protocol::hook_schema_for_handler(handler.as_str())
            .map(|schema| (handler, schema))
    }) {
        Some((handler, schema)) => Integration::Known(Some(Declared {
            handler: handler.as_str().to_owned(),
            schema,
        })),
        None => Integration::Unresolvable,
    }
}

/// The lowest retained digest of the package `candidate` belongs to whose
/// integration differs from `integration`, if any.
///
/// Retained digests that are not installed, belong to another package or are
/// the candidate itself take no part: the first has no declaration to compare
/// and resolves to the uninstalled runtime at launch, and the others do not
/// share the candidate's runtime.
pub(super) fn conflict(
    context: &Context,
    state: &RegistryState,
    retained: &RetainedDigests,
    candidate: (&PackageDigest, &PackageId),
    integration: &Integration,
) -> Option<PackageDigest> {
    let (candidate_digest, package) = candidate;
    let mut integrations = context
        .integrations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    retained
        .iter()
        .filter(|digest| *digest != candidate_digest)
        .filter_map(|digest| state.package(digest))
        .filter(|record| record.identity().id == *package)
        .find(|record| integrations.of(context, record) != *integration)
        .map(|record| record.digest().clone())
}

/// The selection block of `candidate`, or `None` when it is selected or may be
/// selected now.
///
/// `integration` is the declaration of the candidate, taken from a definition
/// the caller already loaded.
pub(super) fn block_of(
    context: &Context,
    state: &RegistryState,
    retained: &RetainedDigests,
    candidate: &PackageRecord,
    integration: &Integration,
) -> Option<PackageSelectionBlock> {
    if state.selected(&candidate.identity().id) == Some(candidate.digest()) {
        return None;
    }
    let identity = (candidate.digest(), &candidate.identity().id);
    conflict(context, state, retained, identity, integration).map(|retained| {
        PackageSelectionBlock {
            reason: PackageSelectionBlockReason::IncompatibleWithRetained,
            retained,
        }
    })
}

/// Whether the installed package `candidate` and the installed package
/// `other` declare different integrations.
///
/// Used when a new pin would make `other`, the selected version, coexist with
/// the pinned `candidate`.
pub(super) fn differs(context: &Context, candidate: &PackageRecord, other: &PackageRecord) -> bool {
    let mut integrations = context
        .integrations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    integrations.of(context, candidate) != integrations.of(context, other)
}

/// Collects the schemas that retained, installed package versions of the
/// runtimes in `state` declare, for the integration update path.
///
/// A retained version whose integration is unresolvable is recorded under its
/// package id so an update of any handler that package serves refuses.
pub(super) fn retained_schemas(
    context: &Context,
    state: &RegistryState,
    retained: &RetainedDigests,
) -> RetainedSchemas {
    let mut integrations = context
        .integrations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut schemas = RetainedSchemas::default();
    for record in retained.iter().filter_map(|digest| state.package(digest)) {
        match integrations.of(context, record) {
            Integration::Known(Some(declared)) => {
                schemas.push(declared.handler, declared.schema);
            }
            Integration::Known(None) => {}
            Integration::Unresolvable => schemas.push_unresolvable(record.identity().id.clone()),
        }
    }
    schemas
}
