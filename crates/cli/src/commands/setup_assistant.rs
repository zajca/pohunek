//! The assistant package step of bare `pohunek setup`: the context package
//! `pohunek.context.assistant` that matches this core release.
//!
//! The package is resolved through the signed plugin index by the resolver
//! `plugin install` uses. The CLI parses the index only to pick an entry and
//! show it in the preview; the daemon verifies the signature, revocations,
//! freshness and archive digest when the package installs. Nothing installs
//! without that verification, so every other outcome is a gap: setup finishes
//! its other steps and then reports `setup_incomplete` with the manual command.

// Rust guideline compliant 2026-11-11

use package::OFFICIAL_CONTEXT_PACKAGE_PREFIX;
use protocol::{method, HostInspectParams, PackageInstallParams, PackageTrust};
use serde::Serialize;

use crate::client::Client;
use crate::commands::plugin;
use crate::commands::plugin_index::{self, PackageRef};
use crate::error::CliError;

/// Package id of the assistant context package.
const ASSISTANT_PACKAGE: &str = "pohunek.context.assistant";

/// The command that installs the package by hand, with the section of
/// `docs/install.md` that explains it.
const MANUAL_COMMAND: &str = "pohunek assistant setup (or `pohunek setup`; docs/install.md, \"Install the assistant package\")";

/// What a setup run does about the assistant package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AssistantStep {
    /// The index lists the package and the apply installs and selects it.
    Install {
        /// The package id.
        package_id: String,
        /// The version the index lists for this core release.
        version: String,
        /// The digest the index lists. The daemon verifies it at install.
        digest: String,
        /// The older version of the package that is selected now and that the
        /// install replaces; absent when no version is selected.
        #[serde(skip_serializing_if = "Option::is_none")]
        replaces: Option<String>,
    },
    /// The package is installed at the listed digest, enabled and selected.
    Present {
        /// The package id.
        package_id: String,
        /// The installed version.
        version: String,
        /// The installed digest.
        digest: String,
    },
    /// The package cannot be installed by this run.
    Gap {
        /// Why, as the failing step reports it.
        detail: String,
        /// The command that installs the package by hand.
        command: String,
    },
}

/// The text of a gap: why the package is missing and the manual command.
fn describe(detail: &str, command: &str) -> String {
    format!("the assistant package is not installed: {detail}; install it by hand with `{command}`")
}

impl AssistantStep {
    fn gap(detail: impl Into<String>) -> Self {
        Self::Gap {
            detail: detail.into(),
            command: MANUAL_COMMAND.to_owned(),
        }
    }
}

/// The request for the assistant package at `version`, or the newest one this
/// core release can use.
pub(crate) fn request(version: Option<&protocol::PackageVersion>) -> Result<PackageRef, CliError> {
    debug_assert!(ASSISTANT_PACKAGE.starts_with(OFFICIAL_CONTEXT_PACKAGE_PREFIX));
    let text = match version {
        Some(version) => format!("{ASSISTANT_PACKAGE}@{version}"),
        None => ASSISTANT_PACKAGE.to_owned(),
    };
    Ok(PackageRef::parse(&text)?)
}

/// Plans the step: resolves the package through the index and, when a daemon
/// answers on `client`, compares it with what is installed.
///
/// Why a plan is a gap: either the index does not list the package for this
/// core release, or something else stopped the resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GapCause {
    /// The official index does not list the package for this core release.
    NotListed,
    /// Something else stopped the resolution.
    Other,
}

/// A plan never fails: whatever stops the resolution is a gap.
pub(crate) async fn plan(index_url: Option<&str>, client: Option<&mut Client>) -> AssistantStep {
    plan_with_cause(index_url, client, None).await.0
}

/// Plans the step and reports why a gap is a gap, so a caller that knows the
/// daemon version can print the concrete QA-a text for a missing listing,
/// matched against that daemon's core version rather than this build's.
///
/// A plan never fails: whatever stops the resolution is a gap.
pub(crate) async fn plan_with_cause(
    index_url: Option<&str>,
    client: Option<&mut Client>,
    daemon_core: Option<&semver::Version>,
) -> (AssistantStep, Option<GapCause>) {
    let Ok(wanted) = request(None) else {
        return (
            AssistantStep::gap("the package reference is invalid"),
            Some(GapCause::Other),
        );
    };
    let resolved = match plugin_index::resolve(&wanted, index_url, daemon_core).await {
        Ok(resolved) => resolved,
        Err(error) => {
            let cause = if plugin_index::is_unlisted_for_core(&error) {
                GapCause::NotListed
            } else {
                GapCause::Other
            };
            return (AssistantStep::gap(error.to_string()), Some(cause));
        }
    };
    let package_id = resolved.id.to_string();
    let version = resolved.version.to_string();
    let digest = resolved.digest.as_str().to_owned();
    let mut replaces = None;
    if let Some(client) = client {
        if let Err(error) = require_context_install(client).await {
            return (AssistantStep::gap(error.to_string()), Some(GapCause::Other));
        }
        if let Ok(listed) = plugin::list(client).await {
            match decide(&listed.packages, &resolved) {
                Decision::Keep(kept) => {
                    return (
                        AssistantStep::Present {
                            package_id,
                            version: kept.package.version.to_string(),
                            digest: kept.digest.as_str().to_owned(),
                        },
                        None,
                    );
                }
                Decision::Revoked(detail) => {
                    return (AssistantStep::gap(detail), Some(GapCause::Other));
                }
                Decision::Held(hold) => {
                    return (
                        AssistantStep::Gap {
                            detail: format!("{}, which setup leaves as it is", hold.detail),
                            command: hold.command,
                        },
                        Some(GapCause::Other),
                    );
                }
                Decision::Install {
                    replaces: selection,
                } => {
                    replaces = selection.map(|info| info.package.version.to_string());
                }
            }
        }
    }
    (
        AssistantStep::Install {
            package_id,
            version,
            digest,
            replaces,
        },
        None,
    )
}

/// What `pohunek setup` and `pohunek service upgrade` do with the assistant
/// package once the index listed a version for this core.
pub(crate) enum Decision<'a> {
    /// A selection that serves and is not older than the listing stays: it is
    /// the assistant the owner runs now.
    Keep(&'a protocol::PackageInfo),
    /// The listed digest is (re)installed; `replaces` is the selection it takes
    /// over, which does not serve or is older than the listing. A listed digest
    /// that is installed with a fault is installed again (the daemon restores
    /// its root) and replaces nothing.
    Install {
        /// The selection the install supersedes.
        replaces: Option<&'a protocol::PackageInfo>,
    },
    /// The listed digest is installed and the owner's disable or selection
    /// choice stands.
    Held(OwnerHold),
    /// The listed digest is revoked; the text says what to wait for.
    Revoked(String),
}

/// The one decision about the assistant package for the version the index
/// `resolved`, read from the daemon's `packages`.
///
/// A serving selection that is not older than the listing is read first, so a
/// digest the owner does not run never replaces a selection that serves. An
/// installed listed digest is never switched by the caller: the owner's choice
/// holds, a revoked one is refused and a faulted one is installed again.
pub(crate) fn decide<'a>(
    packages: &'a [protocol::PackageInfo],
    resolved: &plugin_index::Resolved,
) -> Decision<'a> {
    let package_id = resolved.id.to_string();
    let version = resolved.version.to_string();
    let selected = selected_package(packages, &package_id, &version);
    if let Some(selected) = &selected {
        if serves(selected.info) && !selected.older {
            return Decision::Keep(selected.info);
        }
    }
    let installed = packages
        .iter()
        .find(|info| info.package.id == resolved.id && info.digest == resolved.digest);
    if let Some(info) = installed {
        if info.revoked {
            return Decision::Revoked(format!(
                "version {version} is revoked; wait for a release whose index lists a newer version"
            ));
        }
        if info.fault.is_some() {
            return Decision::Install { replaces: None };
        }
        // `serves` also needs a selected and enabled package, which the
        // selection rule above has already kept, so what is left is the
        // owner's choice.
        let digest = info.digest.as_str();
        let (state, command) = if info.enabled {
            (
                "not selected",
                format!("pohunek plugin select {package_id} --digest {digest}"),
            )
        } else {
            (
                "disabled",
                format!(
                    "pohunek plugin enable {package_id} --digest {digest} && pohunek plugin select {package_id} --digest {digest}"
                ),
            )
        };
        return Decision::Held(OwnerHold {
            detail: format!("version {version} is installed but {state}"),
            command,
        });
    }
    Decision::Install {
        replaces: selected.map(|selected| selected.info),
    }
}

/// The selected package of one id, compared with the version the index lists.
pub(crate) struct SelectedPackage<'a> {
    /// The daemon's entry for the selected package.
    pub(crate) info: &'a protocol::PackageInfo,
    /// Whether its version is older than the listed one.
    pub(crate) older: bool,
}

/// The selected entry of `package_id` among `packages`, with whether it is
/// older than `listed`. This is the one place that decides whether the
/// installed assistant package is current.
pub(crate) fn selected_package<'a>(
    packages: &'a [protocol::PackageInfo],
    package_id: &str,
    listed: &str,
) -> Option<SelectedPackage<'a>> {
    let info = packages
        .iter()
        .find(|info| info.selected && info.package.id.to_string() == package_id)?;
    let older = is_older(info.package.version.as_ref(), listed);
    Some(SelectedPackage { info, older })
}

/// An owner's choice that a service upgrade leaves as it is.
pub(crate) struct OwnerHold {
    /// What the owner set, as a clause.
    pub(crate) detail: String,
    /// The commands that undo the choice.
    pub(crate) command: String,
}

/// The owner's hold on `package_id` among `packages`: it is installed and
/// either no version is selected or the selected version is disabled. A
/// package that is not installed, or whose selected version is enabled, has
/// no hold, so an upgrade may install a newer version of it.
pub(crate) fn owner_hold(
    packages: &[protocol::PackageInfo],
    package_id: &str,
) -> Option<OwnerHold> {
    let installed = || {
        packages
            .iter()
            .filter(|info| info.package.id.to_string() == package_id)
    };
    match installed().find(|info| info.selected) {
        Some(info) if info.enabled => None,
        Some(info) => Some(OwnerHold {
            detail: format!("version {} is selected but disabled", info.package.version),
            command: format!(
                "pohunek plugin enable {package_id} --digest {}",
                info.digest.as_str()
            ),
        }),
        None => {
            let newest = installed().max_by(|left, right| {
                match (
                    semver::Version::parse(left.package.version.as_ref()),
                    semver::Version::parse(right.package.version.as_ref()),
                ) {
                    (Ok(left), Ok(right)) => left.cmp_precedence(&right),
                    _ => std::cmp::Ordering::Equal,
                }
            })?;
            Some(OwnerHold {
                detail: "a version is installed and none is selected".to_owned(),
                command: format!(
                    "pohunek plugin enable {package_id} --digest {digest} && pohunek plugin select {package_id} --digest {digest}",
                    digest = newest.digest.as_str()
                ),
            })
        }
    }
}

/// Whether `info` is a package that serves fresh launches and bare requests.
pub(crate) fn serves(info: &protocol::PackageInfo) -> bool {
    info.enabled && info.selected && info.fault.is_none() && !info.revoked
}

/// Refuses when the daemon cannot authorize a context package through the
/// index.
async fn require_context_install(client: &mut Client) -> Result<(), CliError> {
    let capabilities = client
        .host_inspect_with(HostInspectParams {
            skip_runtime_probes: Some(true),
        })
        .await?;
    if !capabilities.package_install_index_supported {
        return Err(plugin::Error::IndexUnsupported {
            capability: "package_install_index_supported",
        }
        .into());
    }
    if !capabilities.package_install_index_context_supported {
        return Err(plugin::Error::IndexContextUnsupported.into());
    }
    Ok(())
}

/// What the apply did about the assistant package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AssistantOutcome {
    /// Installed and selected by this run.
    Installed(String),
    /// Already installed at the previewed digest.
    Present,
    /// Not installed, with the reason and the manual command.
    Gap(String),
}

/// Applies `step` through the daemon on `client`, matching entries against
/// `daemon_core` (this build's core version when `None`).
///
/// The archive is resolved again at the previewed version. A digest that
/// differs from the previewed one is refused before anything is sent to the
/// daemon, so what installs is what the owner saw.
pub(crate) async fn apply(
    client: &mut Client,
    step: &AssistantStep,
    index_url: Option<&str>,
    daemon_core: Option<&semver::Version>,
) -> AssistantOutcome {
    match step {
        AssistantStep::Present { .. } => AssistantOutcome::Present,
        AssistantStep::Gap { detail, command } => AssistantOutcome::Gap(describe(detail, command)),
        AssistantStep::Install {
            package_id,
            version,
            digest,
            ..
        } => match install(client, version, digest, index_url, daemon_core).await {
            Ok(()) => AssistantOutcome::Installed(format!("package {package_id}@{version}")),
            Err(detail) => AssistantOutcome::Gap(describe(&detail, MANUAL_COMMAND)),
        },
    }
}

/// The command that replaces an older selected package with the version the
/// plugin index lists for this core release.
pub(crate) const UPDATE_COMMAND: &str = "pohunek setup";

/// The package id of the assistant context package.
pub(crate) const PACKAGE_ID: &str = ASSISTANT_PACKAGE;

/// The QA-a text for a gap whose cause is [`GapCause::NotListed`]: the index
/// does not list the assistant package for the daemon's core version.
///
/// The daemon version is rendered in; the text holds no placeholder.
#[must_use]
pub(crate) fn not_listed_text(daemon_version: &str) -> String {
    protocol::ProtocolError::assistant_missing_hint(daemon_version)
}

/// The note for an assistant bundle `bundle` that is older than the core
/// `core`, with the command that updates it; `None` when it is not older or
/// not comparable.
pub(crate) fn outdated_note(bundle: &str, core: &str) -> Option<String> {
    is_older(bundle, core).then(|| {
        format!(
            "the assistant package {ASSISTANT_PACKAGE} {bundle} is older than pohunek {core}; \
             run `{UPDATE_COMMAND}` on the host that runs the session to install the version \
             the plugin index lists for this release (a package you disabled or did not select \
             stays as set and the command names the steps)"
        )
    })
}

/// Whether `current` is a semantic version lower than `listed`; a version
/// that is not semantic is never older.
pub(crate) fn is_older(current: &str, listed: &str) -> bool {
    match (
        semver::Version::parse(current),
        semver::Version::parse(listed),
    ) {
        (Ok(current), Ok(listed)) => current.cmp_precedence(&listed).is_lt(),
        _ => false,
    }
}

/// Installs the previewed package; the error is the detail of the gap.
/// Entries match against `daemon_core` (this build's core version when
/// `None`) so the install stages what the daemon's core can use.
pub(crate) async fn install(
    client: &mut Client,
    version: &str,
    digest: &str,
    index_url: Option<&str>,
    daemon_core: Option<&semver::Version>,
) -> Result<(), String> {
    require_context_install(client)
        .await
        .map_err(|error| error.to_string())?;
    let pinned = protocol::PackageVersion::parse(version)
        .map_err(|error| format!("the previewed version is invalid: {error}"))?;
    let wanted = request(Some(&pinned)).map_err(|error| error.to_string())?;
    let staged = plugin_index::stage(&wanted, index_url, daemon_core)
        .await
        .map_err(|error| error.to_string())?;
    if staged.resolved.digest.as_str() != digest {
        return Err(format!(
            "the index now lists digest {} for {}, not the previewed {digest}; nothing was installed",
            staged.resolved.digest.as_str(),
            staged.resolved.pinned()
        ));
    }
    let installed = client
        .call::<method::PackageInstall>(PackageInstallParams {
            archive_path: staged.archive_path.clone(),
            trust: PackageTrust::Index {
                index_path: staged.index_path.clone(),
            },
            enable: true,
            select: true,
            dry_run: false,
            accept_commands: false,
        })
        .await
        .map_err(|error| error.to_string())?;
    if installed.package.digest.as_str() != digest {
        return Err("the daemon installed a digest other than the previewed one".to_owned());
    }
    let listed = plugin::list(client)
        .await
        .map_err(|error| error.to_string())?;
    let serving = listed
        .packages
        .iter()
        .any(|info| info.digest == staged.resolved.digest && serves(info));
    if serving {
        Ok(())
    } else {
        Err("the package installed but is not enabled and selected".to_owned())
    }
}
