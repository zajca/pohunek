//! `pohunek plugin install --release <dir> --runtime <id>`: the official
//! runtime package of an extracted daemon archive, installed on any host.
//!
//! With a local daemon answering, the install goes over `package.install`
//! with the archive and the signed catalog of the release, so the running
//! daemon stays the only writer of the package registry. Without a daemon
//! (a CLI-only host), the archive is verified as `setup` verifies it and its
//! own `pohunekd ensure-official-runtimes --runtime <id>` installs the
//! package one-shot: the catalog signature, revocations, freshness, digest
//! and claim checks are the daemon's, run by the daemon's code and never
//! reimplemented here. No service is installed and nothing listens.

// Rust guideline compliant 2026-10-14

use std::fmt::Write as _;
use std::io::{self, IsTerminal as _};
use std::path::Path;

use std::collections::BTreeSet;

use pohunek_service_config::runtimes::{
    CatalogStatus, EnsureReport, Gap, PackageAction, PackageStatus, ProfileAction, Refusal,
    CODE_DAEMON_RUNNING, CODE_GAP_AGENT_NOT_ON_PATH, CODE_GAP_UPDATE_SAME_VERSION_OTHER_DIGEST,
    RELEASE_RUNTIME_DIR,
};
use protocol::{PackageTrust, RuntimeId};
use serde::Serialize;

use super::{canonical_path, connect_ingest, ingest_package, Ingest, InstallArgs, Source};
use crate::commands::render_json;
use crate::commands::setup_host::{
    confirm, pending_blocked_detail, refusal_text, verify_sealed_archive,
};
use crate::error::CliError;
use crate::service::runtimes::DaemonRuntimes;
use crate::service::{layout, Context, VERSION};

/// Directory of the release's package archives below `runtime/`.
const PACKAGES_DIR: &str = "packages";

/// Suffix of a release package archive.
const ARCHIVE_SUFFIX: &str = ".tar.zst";

/// Name of the release's signed catalog below `runtime/`.
const CATALOG_FILE: &str = "runtime-catalog.json";

/// The name the release daemon answers `--version` with.
const DAEMON_NAME: &str = "pohunekd";

/// Route of a result: the install ran in a one-shot release daemon step.
const ROUTE_OFFLINE: &str = "offline";

/// The result `--json` prints for the offline route.
#[derive(Debug, Serialize)]
struct ReleaseInstall {
    /// How the install ran.
    route: &'static str,
    /// The release directory the package came from.
    release: String,
    /// The plan the consent covered.
    preview: Preview,
    /// The packages the step installed or found present.
    packages: Vec<PackageAction>,
    /// The host profiles the step pinned or left.
    profiles: Vec<ProfileAction>,
    /// What the step reports without refusing.
    gaps: Vec<Gap>,
}

/// Everything the apply may change, as the dry run reports it.
#[derive(Debug, Serialize)]
struct Preview {
    /// Packages the step installs or finds present.
    packages: Vec<PackageAction>,
    /// Host profiles the step binds or repins.
    profiles: Vec<ProfileAction>,
    /// The catalog trust state the step records.
    catalog: CatalogStatus,
    /// What the step reports without refusing.
    gaps: Vec<Gap>,
}

/// Installs the requested runtime of `args.release`.
///
/// # Errors
///
/// Returns [`CliError`] for an unusable release directory, a refused plan, a
/// missing consent, a failed apply, or the daemon route's own errors.
pub(super) async fn run(args: &InstallArgs) -> Result<(), CliError> {
    let release = args
        .release
        .as_deref()
        .expect("clap requires --release for this route");
    let runtime = args
        .runtime
        .as_ref()
        .expect("clap requires --runtime with --release");
    let dir = canonical_path(release, "directory", Path::is_dir)?;
    let runtime_dir = Path::new(&dir).join(RELEASE_RUNTIME_DIR);
    match connect_ingest().await {
        Ok(mut client) => {
            let archive = runtime_dir
                .join(PACKAGES_DIR)
                .join(format!("{runtime}{ARCHIVE_SUFFIX}"));
            let source = Source::Archive {
                path: canonical_path(&archive, "file", Path::is_file)?,
                trust: PackageTrust::Catalog {
                    catalog_path: canonical_path(
                        &runtime_dir.join(CATALOG_FILE),
                        "file",
                        Path::is_file,
                    )?,
                },
            };
            let ingest = Ingest::Install {
                enable: !args.no_enable,
            };
            ingest_package(
                &mut client,
                &source,
                ingest,
                None,
                None,
                args.yes,
                args.json,
            )
            .await
        }
        Err(CliError::DaemonUnreachable { source, .. }) if no_daemon(&source) => {
            offline(args, Path::new(&dir), runtime).await
        }
        Err(error) => Err(error),
    }
}

/// Whether a failed connect means that no daemon listens: the socket is
/// absent or refuses. Any other failure (permissions, a timeout) may be a
/// live daemon, so it is surfaced instead of installing behind its back.
fn no_daemon(source: &io::Error) -> bool {
    matches!(
        source.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

/// The release in `dir`, once nothing else owns the package registry and the
/// archive is sealed, intact and of this CLI's version.
async fn verified_release(
    context: &Context,
    dir: &Path,
) -> Result<layout::VerifiedVersionDir, CliError> {
    let blocked = |detail: String| CliError::ReleaseInstallBlocked { detail };
    let status = crate::service::installed_facts(context)?;
    if let Some(pending) = &status.pending_transaction {
        return Err(blocked(pending_blocked_detail(pending)));
    }
    if status.installed && status.active_version.as_deref() != Some(VERSION) {
        return Err(blocked(format!(
            "the installed service runs version {}, this pohunek is {VERSION}; run \
             `pohunek service upgrade` first, so one release owns the package registry",
            status.active_version.as_deref().unwrap_or("unknown")
        )));
    }
    let sealed = verify_sealed_archive(dir, context.uid())
        .await
        .map_err(|error| match error {
            CliError::SetupBlocked { detail } => blocked(detail),
            other => other,
        })?
        .ok_or_else(|| {
            blocked(format!(
                "{} is not an extracted daemon archive: it has neither a MANIFEST nor a \
                 packaging directory; extract the release archive of this version and name it",
                dir.display()
            ))
        })?;
    let verified = layout::sealed_release(&sealed);
    layout::probe(&verified.daemon(), DAEMON_NAME, VERSION)
        .await
        .map_err(|error| {
            blocked(format!(
                "the release does not match this pohunek {VERSION}: {error}; use the daemon \
                 archive of the same version"
            ))
        })?;
    Ok(verified)
}

/// The install on a host without a daemon.
async fn offline(args: &InstallArgs, dir: &Path, runtime: &RuntimeId) -> Result<(), CliError> {
    let blocked = |detail: String| CliError::ReleaseInstallBlocked { detail };
    if !args.yes && (args.json || !io::stdin().is_terminal()) {
        return Err(CliError::ReleaseInstallConsentRequired);
    }
    let context = Context::resolve().map_err(|error| blocked(error.to_string()))?;
    let verified = verified_release(&context, dir).await?;
    let search_path = std::env::var("PATH").map_err(|_unset| {
        blocked(
            "PATH is not set to valid UTF-8, so the release step cannot judge the agent".to_owned(),
        )
    })?;
    let steps = DaemonRuntimes::default();
    let requested = std::slice::from_ref(runtime);
    let plan = steps
        .run_requested(&context, &verified, true, &search_path, requested)
        .await
        .map_err(|error| blocked(error.to_string()))?;
    if !plan.refusals.is_empty() {
        return Err(blocked(refusal_text(&plan.refusals)));
    }
    refuse_unless_installable(&plan, runtime)?;
    if !args.json {
        print!("{}", render(&plan, "plan"));
    }
    if !args.yes && !confirm()? {
        return Err(CliError::ReleaseInstallDeclined);
    }
    let applied = steps
        .run_requested(&context, &verified, false, &search_path, requested)
        .await
        .map_err(|error| CliError::ReleaseInstallIncomplete {
            detail: error.to_string(),
        })?;
    if applied
        .refusals
        .iter()
        .any(|refusal| refusal.code == CODE_DAEMON_RUNNING)
    {
        return Err(blocked(format!(
            "{}; stop the daemon, or install over its socket once it answers, then re-run",
            refusal_text(&applied.refusals)
        )));
    }
    ensure_no_refusal(&applied.refusals)?;
    if !matches_preview(&plan, &applied) {
        return Err(CliError::ReleaseInstallIncomplete {
            detail: "the apply changed packages or profiles other than the previewed ones; \
                     inspect `pohunek plugin list` and the host profiles"
                .to_owned(),
        });
    }
    let verdict = steps
        .run_requested(&context, &verified, true, &search_path, requested)
        .await
        .map_err(|error| CliError::ReleaseInstallIncomplete {
            detail: error.to_string(),
        })?;
    ensure_no_refusal(&verdict.refusals)?;
    let present = verdict.packages.iter().any(|package| {
        package.runtime == runtime.as_str() && package.status == PackageStatus::Present
    });
    if !present {
        return Err(CliError::ReleaseInstallIncomplete {
            detail: format!("the verifying plan does not list `{runtime}` as present"),
        });
    }
    if args.json {
        let result = ReleaseInstall {
            route: ROUTE_OFFLINE,
            release: dir.display().to_string(),
            preview: Preview {
                packages: plan.packages,
                profiles: plan.profiles,
                catalog: plan.catalog,
                gaps: plan.gaps,
            },
            packages: verdict.packages,
            profiles: applied.profiles,
            gaps: verdict.gaps,
        };
        print!("{}", render_json(&result)?);
    } else {
        print!("{}", render(&verdict, "installed"));
    }
    Ok(())
}

/// Refuses, before any consent, a plan that would not leave the requested
/// runtime installed and selected: the release's package is kept behind an
/// installed one (for example the same version under another digest).
fn refuse_unless_installable(plan: &EnsureReport, runtime: &RuntimeId) -> Result<(), CliError> {
    let installable = plan.packages.iter().any(|package| {
        package.runtime == runtime.as_str()
            && matches!(
                package.status,
                PackageStatus::Install | PackageStatus::Present
            )
    });
    if installable {
        return Ok(());
    }
    let reasons: Vec<String> = plan
        .gaps
        .iter()
        .filter(|gap| gap.runtime == runtime.as_str() && gap.code != CODE_GAP_AGENT_NOT_ON_PATH)
        .map(|gap| format!("{} ({})", gap.detail, gap.code))
        .collect();
    let same_version = plan.gaps.iter().any(|gap| {
        gap.runtime == runtime.as_str() && gap.code == CODE_GAP_UPDATE_SAME_VERSION_OTHER_DIGEST
    });
    let next = if same_version {
        "use a release whose package has a new version"
    } else {
        "resolve the reported condition, then re-run"
    };
    Err(CliError::ReleaseInstallBlocked {
        detail: format!(
            "this release would not install `{runtime}`: {}; {next}",
            if reasons.is_empty() {
                "an installed package is kept".to_owned()
            } else {
                reasons.join("; ")
            }
        ),
    })
}

/// Whether the apply changed exactly the packages and profiles the plan
/// showed.
fn matches_preview(plan: &EnsureReport, applied: &EnsureReport) -> bool {
    let packages = |report: &EnsureReport, status: PackageStatus| -> BTreeSet<(String, String)> {
        report
            .packages
            .iter()
            .filter(|package| package.status == status)
            .map(|package| (package.runtime.clone(), package.digest.clone()))
            .collect()
    };
    let profiles = |report: &EnsureReport, applied: bool| -> BTreeSet<(String, String)> {
        report
            .profiles
            .iter()
            .filter(|profile| {
                if applied {
                    profile.status.is_applied()
                } else {
                    profile.status.is_pending()
                }
            })
            .map(|profile| (profile.name.clone(), profile.digest.clone()))
            .collect()
    };
    packages(plan, PackageStatus::Install) == packages(applied, PackageStatus::Installed)
        && profiles(plan, false) == profiles(applied, true)
}

/// Fails an apply or a verdict that carries refusals.
fn ensure_no_refusal(refusals: &[Refusal]) -> Result<(), CliError> {
    if refusals.is_empty() {
        Ok(())
    } else {
        Err(CliError::ReleaseInstallIncomplete {
            detail: refusal_text(refusals),
        })
    }
}

/// The packages and gaps of `report`, one line each, under `heading`.
fn render(report: &EnsureReport, heading: &str) -> String {
    let mut text = String::new();
    for package in &report.packages {
        let _ = writeln!(
            text,
            "{heading}: {} {} {}  {}",
            package.runtime,
            package.package,
            package.version,
            serde_json::to_value(package.status)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default()
        );
    }
    for profile in &report.profiles {
        let _ = writeln!(
            text,
            "{heading}: profile {} ({}) -> {}  {}",
            profile.name,
            profile.base,
            profile.digest,
            serde_json::to_value(profile.status)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default()
        );
    }
    let _ = writeln!(
        text,
        "{heading}: catalog trust state {}",
        serde_json::to_value(report.catalog)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default()
    );
    for gap in &report.gaps {
        let _ = writeln!(text, "gap: {}: {} ({})", gap.runtime, gap.detail, gap.code);
    }
    text
}
