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
const MANUAL_COMMAND: &str = "pohunek plugin install ./assistant.tar.zst --sha256 <digest-from-the-release-notes> --yes (docs/install.md, \"Install the assistant package\")";

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
fn request(version: Option<&protocol::PackageVersion>) -> Result<PackageRef, CliError> {
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
/// A plan never fails: whatever stops the resolution is a gap.
pub(crate) async fn plan(index_url: Option<&str>, client: Option<&mut Client>) -> AssistantStep {
    let Ok(wanted) = request(None) else {
        return AssistantStep::gap("the package reference is invalid");
    };
    let resolved = match plugin_index::resolve(&wanted, index_url).await {
        Ok(resolved) => resolved,
        Err(error) => return AssistantStep::gap(error.to_string()),
    };
    let package_id = resolved.id.to_string();
    let version = resolved.version.to_string();
    let digest = resolved.digest.as_str().to_owned();
    let mut replaces = None;
    if let Some(client) = client {
        if let Err(error) = require_context_install(client).await {
            return AssistantStep::gap(error.to_string());
        }
        if let Ok(listed) = plugin::list(client).await {
            // A selection that serves and is not older than the listing stays:
            // it is the assistant the owner runs now. This rule is read before
            // the listed digest, so a digest the owner does not run never
            // replaces a selection that serves.
            let selected = listed
                .packages
                .iter()
                .find(|info| info.package.id == resolved.id && info.selected);
            if let Some(selected) = selected {
                let current = selected.package.version.to_string();
                if serves(selected) && !is_older(&current, &version) {
                    return AssistantStep::Present {
                        package_id,
                        version: current,
                        digest: selected.digest.as_str().to_owned(),
                    };
                }
            }
            if let Some(info) = listed
                .packages
                .iter()
                .find(|info| info.package.id == resolved.id && info.digest == resolved.digest)
            {
                if serves(info) {
                    return AssistantStep::Present {
                        package_id,
                        version,
                        digest,
                    };
                }
                if info.revoked {
                    return AssistantStep::gap(format!(
                        "version {version} is revoked; wait for a release whose index lists a newer version"
                    ));
                }
                // A package with a fault (a missing root, say) is installed
                // again: the daemon restores the root of the same digest.
                if info.fault.is_some() {
                    return AssistantStep::Install {
                        package_id,
                        version,
                        digest,
                        replaces: None,
                    };
                }
                // An owner's disable or version choice is not overridden by
                // a reinstall of the same digest, which the daemon keeps as it is.
                return AssistantStep::Gap {
                    detail: format!(
                        "version {version} is installed but not enabled and selected, which setup leaves as it is"
                    ),
                    command: format!(
                        "pohunek plugin enable {package_id} --digest {digest} && pohunek plugin select {package_id} --digest {digest}"
                    ),
                };
            }
            // The listed digest is not installed, so installing it replaces a
            // selection that does not serve or is older than the listing.
            if let Some(selected) = selected {
                replaces = Some(selected.package.version.to_string());
            }
        }
    }
    AssistantStep::Install {
        package_id,
        version,
        digest,
        replaces,
    }
}

/// Whether `info` is a package that serves fresh launches and bare requests.
fn serves(info: &protocol::PackageInfo) -> bool {
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
        return Err(plugin::Error::IndexUnsupported.into());
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

/// Applies `step` through the daemon on `client`.
///
/// The archive is resolved again at the previewed version. A digest that
/// differs from the previewed one is refused before anything is sent to the
/// daemon, so what installs is what the owner saw.
pub(crate) async fn apply(
    client: &mut Client,
    step: &AssistantStep,
    index_url: Option<&str>,
) -> AssistantOutcome {
    match step {
        AssistantStep::Present { .. } => AssistantOutcome::Present,
        AssistantStep::Gap { detail, command } => AssistantOutcome::Gap(describe(detail, command)),
        AssistantStep::Install {
            package_id,
            version,
            digest,
            ..
        } => match install(client, version, digest, index_url).await {
            Ok(()) => AssistantOutcome::Installed(format!("package {package_id}@{version}")),
            Err(detail) => AssistantOutcome::Gap(describe(&detail, MANUAL_COMMAND)),
        },
    }
}

/// Whether `current` is a semantic version lower than `listed`; a version
/// that is not semantic is never older.
fn is_older(current: &str, listed: &str) -> bool {
    match (
        semver::Version::parse(current),
        semver::Version::parse(listed),
    ) {
        (Ok(current), Ok(listed)) => current.cmp_precedence(&listed).is_lt(),
        _ => false,
    }
}

/// Installs the previewed package; the error is the detail of the gap.
async fn install(
    client: &mut Client,
    version: &str,
    digest: &str,
    index_url: Option<&str>,
) -> Result<(), String> {
    require_context_install(client)
        .await
        .map_err(|error| error.to_string())?;
    let pinned = protocol::PackageVersion::parse(version)
        .map_err(|error| format!("the previewed version is invalid: {error}"))?;
    let wanted = request(Some(&pinned)).map_err(|error| error.to_string())?;
    let staged = plugin_index::stage(&wanted, index_url)
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
