//! The assistant package step of `pohunek service upgrade`: after a committed
//! core upgrade it installs the `pohunek.context.assistant` version the signed
//! plugin index lists for the new core.
//!
//! The step resolves the package through the same index path `pohunek setup`
//! uses, so the daemon still verifies the signature, revocations, freshness and
//! digest. Every outcome is reported and none fails or rolls back the upgrade:
//! the core is already committed when the step runs.

// Rust guideline compliant 2026-11-21

use crate::client::Client;
use crate::commands::plugin::{self, Error};
use crate::commands::plugin_index;
use crate::commands::service::assistant_state;
use crate::commands::setup_assistant::{self, PACKAGE_ID, UPDATE_COMMAND};
use crate::error::CliError;
use crate::paths::Paths;
use crate::service::report::{
    AssistantInstall, AssistantInstallResult, AssistantPackageStatus, UpgradeReport,
};
use crate::target::LOCAL_HOST;

/// What the caller decided about the step.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options<'a> {
    /// `--no-assistant-package`: ask no index and install nothing.
    pub opt_out: bool,
    /// `--repair`: the run restores a version directory and installs nothing.
    pub repair: bool,
    /// The plugin index to read; the official one when `None`.
    pub index_url: Option<&'a str>,
}

/// `report` with the assistant package state of the upgraded installation,
/// after the install the gate allows. Never returns an error.
pub(crate) async fn reconcile(
    mut report: UpgradeReport,
    paths: Result<Paths, CliError>,
    options: &Options<'_>,
) -> UpgradeReport {
    let core = report.to_version.clone();
    let paths = match paths {
        Ok(paths) => paths,
        Err(error) => {
            report.assistant = Some(assistant_state(&core, Err(error)).await);
            return report;
        }
    };
    let before = assistant_state(&core, Ok(paths.clone())).await;
    let install = attempt(
        &report,
        &before.status,
        before.version.as_deref(),
        &paths,
        options,
    )
    .await;
    // An attempt that failed after the daemon installed the package still
    // leaves the daemon's state as the report's truth.
    let attempted = install.as_ref().is_some_and(|install| {
        matches!(
            install.result,
            AssistantInstallResult::Installed | AssistantInstallResult::Failed
        )
    });
    let mut state = if attempted {
        assistant_state(&core, Ok(paths)).await
    } else {
        before
    };
    state.install = install;
    report.assistant = Some(state);
    report
}

/// Whether the upgrade moved the core forward to this build's version, the
/// only run whose package the index can be asked for.
fn moved_forward(report: &UpgradeReport, repair: bool) -> bool {
    let (Ok(from), Ok(to)) = (
        semver::Version::parse(&report.from_version),
        semver::Version::parse(&report.to_version),
    ) else {
        return false;
    };
    let build = semver::Version::parse(crate::service::VERSION).ok();
    !report.unchanged
        && !repair
        && build.is_some_and(|build| build == to)
        && to.cmp_precedence(&from).is_gt()
}

/// What the step did, or `None` when it had nothing to do.
async fn attempt(
    report: &UpgradeReport,
    status: &AssistantPackageStatus,
    selected_version: Option<&str>,
    paths: &Paths,
    options: &Options<'_>,
) -> Option<AssistantInstall> {
    if *status == AssistantPackageStatus::Unchecked {
        return None;
    }
    if options.opt_out {
        return Some(outcome(
            AssistantInstallResult::Skipped,
            Some("--no-assistant-package was given".to_owned()),
            Some(UPDATE_COMMAND.to_owned()),
        ));
    }
    if !moved_forward(report, options.repair) || *status == AssistantPackageStatus::Current {
        return None;
    }
    Some(install(&report.to_version, selected_version, paths, options).await)
}

/// Installs the version the index lists for `core` unless the owner holds the
/// package or the index lists nothing newer than `selected_version`.
async fn install(
    core: &str,
    selected_version: Option<&str>,
    paths: &Paths,
    options: &Options<'_>,
) -> AssistantInstall {
    let mut client = match Client::connect(LOCAL_HOST, paths).await {
        Ok(client) => client,
        Err(error) => return failed(error.to_string()),
    };
    let listed = match plugin::list(&mut client).await {
        Ok(listed) => listed,
        Err(error) => return failed(error.to_string()),
    };
    if let Some(hold) = setup_assistant::owner_hold(&listed.packages, PACKAGE_ID) {
        return outcome(
            AssistantInstallResult::Held,
            Some(format!(
                "{}, which the upgrade leaves as it is",
                hold.detail
            )),
            Some(hold.command),
        );
    }
    let selected = listed
        .packages
        .iter()
        .find(|info| info.selected && info.package.id.to_string() == PACKAGE_ID);
    let wanted = match setup_assistant::request(None) {
        Ok(wanted) => wanted,
        Err(error) => return failed(error.to_string()),
    };
    let resolved = match plugin_index::resolve(&wanted, options.index_url, None).await {
        Ok(resolved) => resolved,
        Err(error)
            if plugin_index::is_unlisted_for_core(&error)
                || matches!(error, Error::IndexPlatformUnsupported { .. }) =>
        {
            // No entry fits this core or this platform: the index does not
            // publish the package for this host yet.
            return not_published(core, selected_version);
        }
        Err(error) => return failed(error.to_string()),
    };
    let version = resolved.version.to_string();
    if selected_version.is_some_and(|current| !setup_assistant::is_older(current, &version)) {
        return not_published(core, selected_version);
    }
    let digest = resolved.digest.as_str().to_owned();
    let installed = listed.packages.iter().find(|info| {
        info.package.id == resolved.id
            && info.digest == resolved.digest
            && info.fault.is_none()
            && !info.revoked
    });
    if let Some(held) = installed {
        // The listed digest is already installed: a daemon install of it keeps
        // the recorded state, so only the owner's `plugin select` changes the
        // selection, as after `pohunek setup`.
        let (state, command) = if held.enabled {
            (
                "not selected",
                format!("pohunek plugin select {PACKAGE_ID} --digest {digest}"),
            )
        } else {
            (
                "disabled",
                format!(
                    "pohunek plugin enable {PACKAGE_ID} --digest {digest} && pohunek plugin select {PACKAGE_ID} --digest {digest}"
                ),
            )
        };
        return outcome(
            AssistantInstallResult::Held,
            Some(format!(
                "version {} is installed but {state}, which the upgrade leaves as it is",
                held.package.version
            )),
            Some(command),
        );
    }
    match setup_assistant::install(&mut client, &version, &digest, options.index_url, None).await {
        Ok(()) => installed_outcome(version, digest, selected),
        Err(detail) => failed(detail),
    }
}

fn installed_outcome(
    version: String,
    digest: String,
    replaced: Option<&protocol::PackageInfo>,
) -> AssistantInstall {
    AssistantInstall {
        result: AssistantInstallResult::Installed,
        version: Some(version),
        digest: Some(digest),
        replaced_version: replaced.map(|info| info.package.version.to_string()),
        replaced_digest: replaced.map(|info| info.digest.as_str().to_owned()),
        detail: None,
        command: None,
    }
}

fn outcome(
    result: AssistantInstallResult,
    detail: Option<String>,
    command: Option<String>,
) -> AssistantInstall {
    AssistantInstall {
        result,
        version: None,
        digest: None,
        replaced_version: None,
        replaced_digest: None,
        detail,
        command,
    }
}

fn failed(detail: String) -> AssistantInstall {
    outcome(
        AssistantInstallResult::Failed,
        Some(detail),
        Some(UPDATE_COMMAND.to_owned()),
    )
}

fn not_published(core: &str, selected_version: Option<&str>) -> AssistantInstall {
    let newer =
        selected_version.map_or_else(String::new, |version| format!(" newer than {version}"));
    outcome(
        AssistantInstallResult::NotPublished,
        Some(format!(
            "the plugin index lists no {PACKAGE_ID}{newer} for pohunek {core}"
        )),
        Some(UPDATE_COMMAND.to_owned()),
    )
}
