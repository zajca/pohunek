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
        // A package that exports commands only declares no integration.
        Err(PackageRejection::RuntimeMissing) => Integration::Known(None),
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

/// The integration of the installed package `record`.
pub(super) fn integration_of(context: &Context, record: &PackageRecord) -> Integration {
    context
        .integrations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .of(context, record)
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

#[cfg(test)]
mod tests {
    //! A descriptor written before `[integration] hook_schema` existed can only
    //! reach the registry directly (an install refuses it), so its resolution
    //! is observed against a plugin root and a package context built here.

    use std::path::Path;
    use std::sync::Mutex;

    use package::registry::{InstallRequest, PackageSource};
    use package::{build_archive, read_archive, ArchiveEntry, Limits};

    use super::*;
    use crate::agent::host::definition_from_archive;
    use crate::agent::host::fixture::{
        config_home_table, installed_pi_host, pi_shaped_document, PI_SHAPED_NO_CHECK,
        PI_SHAPED_PACKAGE_ID,
    };
    use crate::catalog_anchor::CatalogTrust;
    use crate::test_support::thread_scoped_dir;

    /// Handler and schema of an integration that reports subagents.
    const SUBAGENT: (&str, &str) = ("codex-hook-v1", "identity-subagent-v1");

    /// Handler and schema of an integration that reports identity only.
    const IDENTITY: (&str, &str) = ("hermes-hook-v1", "identity-v1");

    /// A program that is never launched.
    const INERT_PROGRAM: &str = "/bin/sh";

    /// Detection manifest every fixture archive ships.
    const DETECT_MANIFEST: &str = "[[rules]]\nid = \"idle_prompt\"\nstate = \"idle\"\npriority = 100\nregion = \"whole_recent\"\nany = [{ contains = \"ready\" }]\n";

    fn archive_of(version: &str, integration: &str) -> (Vec<u8>, PackageDigest) {
        let document = pi_shaped_document(Path::new(INERT_PROGRAM), PI_SHAPED_NO_CHECK)
            .replace("version = \"1.0.0\"", &format!("version = \"{version}\""))
            .replace(
                "detect_manifest = \"any\"",
                "detect_manifest = \"detect.toml\"",
            );
        let entries = [
            ArchiveEntry {
                path: "runtime.toml".to_owned(),
                contents: format!("{document}\n{integration}").into_bytes(),
                executable: false,
            },
            ArchiveEntry {
                path: "detect.toml".to_owned(),
                contents: DETECT_MANIFEST.as_bytes().to_vec(),
                executable: false,
            },
        ];
        let bytes = build_archive(&entries, &Limits::DEFAULT).expect("the archive builds");
        let digest = read_archive(&bytes, &Limits::DEFAULT)
            .expect("the archive reads")
            .digest()
            .clone();
        (bytes, digest)
    }

    fn legacy(version: &str, handler: &str) -> (Vec<u8>, PackageDigest) {
        archive_of(
            version,
            &format!("[integration]\nhandler = \"{handler}\"\n"),
        )
    }

    fn current(version: &str, (handler, schema): (&str, &str)) -> RuntimeDefinition {
        let integration = format!(
            "[integration]\nhandler = \"{handler}\"\nhook_schema = \"{schema}\"\n{}",
            config_home_table(handler)
        );
        let (bytes, digest) = archive_of(version, &integration);
        let archive = read_archive(&bytes, &Limits::DEFAULT).expect("the archive reads");
        definition_from_archive(archive.entries(), &digest).expect("definition")
    }

    #[test]
    fn a_descriptor_that_predates_the_hook_schema_resolves_through_its_handler() {
        let plugins = thread_scoped_dir("pohunek-selection-legacy-").join("plugins");
        let (host, installed) = installed_pi_host(&plugins, Path::new(INERT_PROGRAM));
        let store = host.package_store().expect("store").clone();
        let known = legacy("1.1.0", SUBAGENT.0);
        let ambiguous = legacy("1.2.0", "acme-unknown-v1");
        for (bytes, digest) in [&known, &ambiguous] {
            store
                .registry()
                .install(&InstallRequest {
                    archive: bytes,
                    expected: digest,
                    identity: protocol::PackageIdentity {
                        id: PackageId::parse(PI_SHAPED_PACKAGE_ID).expect("package id"),
                        version: protocol::PackageVersion::parse(if digest == &known.1 {
                            "1.1.0"
                        } else {
                            "1.2.0"
                        })
                        .expect("version"),
                    },
                    source: PackageSource::ExplicitDigest,
                    enabled: true,
                    select: false,
                    installed_at_unix_seconds: 1,
                })
                .expect("install the pre-schema package");
        }
        let state = store.registry().state().expect("state");
        let context = Context {
            store,
            runtimes: host,
            trust: CatalogTrust::Absent,
            integrations: Mutex::default(),
        };
        let mut integrations = Integrations::default();
        let mut resolve = |digest: &PackageDigest| {
            integrations.of(&context, state.package(digest).expect("installed"))
        };

        let resolved = resolve(&known.1);
        assert!(
            matches!(&resolved, Integration::Known(Some(Declared { .. }))),
            "{resolved:?}"
        );
        assert_eq!(
            resolve(&ambiguous.1),
            Integration::Unresolvable,
            "a handler no schema is driven by is unresolvable"
        );

        // A retained pre-schema version is compatible with the integration its
        // handler resolves to and incompatible with any other.
        let retained = RetainedDigests::from_iter([known.1.clone()]);
        let package = PackageId::parse(PI_SHAPED_PACKAGE_ID).expect("package id");
        let same = current("2.0.0", SUBAGENT);
        let other = current("2.1.0", IDENTITY);
        let candidate = |definition: &RuntimeDefinition, digest: &PackageDigest| {
            conflict(
                &context,
                &state,
                &retained,
                (digest, &package),
                &declared_by(definition),
            )
        };
        assert_eq!(candidate(&same, &installed), None);
        assert_eq!(candidate(&other, &installed), Some(known.1));
    }
}
