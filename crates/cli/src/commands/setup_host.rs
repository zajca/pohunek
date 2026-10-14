//! `pohunek setup` — the one command that makes this host ready to run pohunek.
//!
//! The run has six steps, in this order:
//!
//! 1. Service: without an installed service the release binaries next to the
//!    running `pohunek` are installed. The same version is kept as it is. A
//!    different installed version is a gap; setup never upgrades.
//! 2. Plan: the release's `pohunekd ensure-official-runtimes --dry-run --detect`
//!    reads the catalog and reports the packages, profiles, integrations and
//!    gaps. It only reads, so it runs before the preview and before any write.
//! 3. Preview and consent: every effect and gap is shown first. `--yes` runs
//!    unattended; without a terminal or with `--json` the run is refused.
//! 4. Apply, online through the daemon's RPCs: packages, profile pins, hooks.
//! 5. Config: the non-forcing `setup config` writer; existing files are kept.
//! 6. Verdict: a second plan, the daemon's health and `doctor`.
//!
//! Before a fresh install the plan reads the staged release directory, and
//! after the install it must reproduce that plan from the installed version
//! directory. A difference stops the run before any package, profile, hook or
//! config write, so the owner only ever applies what the preview showed.

// Rust guideline compliant 2026-10-14

use std::fmt::Write as _;
use std::io::{self, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};

use pohunek_service_config::runtimes::{
    EnsureReport, Gap, IntegrationAction, PackageAction, PackageStatus, ProfileAction,
    ProfileStatus, Refusal, RELEASE_RUNTIME_DIR,
};
use protocol::{
    method, DoctorStatus, PackageBindProfileParams, PackageDigest, PackageInstallParams,
    PackageTrust,
};
use serde::Serialize;

use crate::client::Client;
use crate::commands::doctor;
use crate::commands::health;
use crate::commands::integration::{reinstall_after_upgrade, IntegrationEnv};
use crate::commands::setup::{config_files, install_config, ConfigResult};
use crate::error::CliError;
use crate::paths::Paths;
use crate::service::engine::{install_layout, installed_search_path};
use crate::service::report::{IntegrationReinstall, StatusReport};
use crate::service::runtimes::DaemonRuntimes;
use crate::service::{self, Context, VERSION};
use crate::target::LOCAL_HOST;

/// Name of the signed catalog inside the release's `runtime/` directory.
///
/// Matches `CATALOG_FILE` of the release daemon's `ensure-official-runtimes`,
/// which reads the same directory.
const RELEASE_CATALOG_FILE: &str = "runtime-catalog.json";

/// Directory of the release's package archives inside `runtime/`.
///
/// Matches `PACKAGES_DIR` of the release daemon's `ensure-official-runtimes`.
const RELEASE_PACKAGES_DIR: &str = "packages";

/// Suffix of a release package archive, named after the runtime id.
///
/// Matches `ARCHIVE_SUFFIX` of the release daemon's `ensure-official-runtimes`.
const RELEASE_ARCHIVE_SUFFIX: &str = ".tar.zst";

/// Runtime id whose Hermes homes the CLI reinstalls itself, offline.
const HERMES_RUNTIME: &str = "hermes";

/// Hook status of a home whose hook was installed.
const HOOK_INSTALLED: &str = "installed";

/// Answers, compared case-insensitively, that confirm the plan at the prompt.
const CONFIRM_ANSWERS: [&str; 2] = ["y", "yes"];

/// Question asked once after the preview, on the terminal's error stream.
const CONFIRM_PROMPT: &str = "Apply this plan? [y/N] ";

/// The service step of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(crate) enum ServiceStep {
    /// No service is installed: this release is installed from `from`.
    Install {
        /// The directory the release binaries are copied from.
        from: String,
    },
    /// The installed service runs this release already; nothing to do.
    Present,
}

/// The final checks of a run.
#[derive(Debug, Serialize)]
pub(crate) struct Verdict {
    /// Runtimes whose planned package is not present after the apply.
    pub(crate) packages_missing: Vec<String>,
    /// Hook homes that are not installed: a failed install or a gap.
    pub(crate) hooks_pending: Vec<String>,
    /// The daemon's health status.
    pub(crate) health: String,
    /// The overall status of `doctor`.
    pub(crate) doctor: String,
}

/// What a completed run did and found.
#[derive(Debug, Serialize)]
pub(crate) struct SetupReport {
    /// The service step that ran.
    pub(crate) service: ServiceStep,
    /// One line per change made: the package, profile and hook outcomes.
    pub(crate) applied: Vec<String>,
    /// The planned packages, as previewed.
    pub(crate) packages: Vec<PackageAction>,
    /// The planned profile pins, as previewed.
    pub(crate) profiles: Vec<ProfileAction>,
    /// The planned integrations, as previewed.
    pub(crate) integrations: Vec<IntegrationAction>,
    /// The gaps that remain, as previewed.
    pub(crate) gaps: Vec<Gap>,
    /// The outcome of every integration hook.
    pub(crate) hooks: Vec<IntegrationReinstall>,
    /// The config files written and kept.
    pub(crate) config: ConfigResult,
    /// The final checks.
    pub(crate) verdict: Verdict,
}

/// The read-only plan of a run, before anything is written.
struct Plan {
    /// The service step the plan assumes.
    service: ServiceStep,
    /// The release directory the plan was read from.
    dir: PathBuf,
    /// The `PATH` the daemon job runs with.
    search_path: String,
    /// What the release daemon reported for `dir`.
    report: EnsureReport,
    /// Each config file with whether it already exists.
    config_files: Vec<(PathBuf, bool)>,
}

/// Run bare `pohunek setup`.
///
/// # Errors
///
/// Returns [`CliError::SetupConsentRequired`] without `--yes` when no
/// confirmation can be asked, [`CliError::SetupDeclined`] when the owner says
/// no, [`CliError::SetupBlocked`] before any change, and
/// [`CliError::SetupIncomplete`] when a step after the first change fails.
pub(crate) async fn run(paths: &Paths, yes: bool, json: bool) -> Result<(), CliError> {
    if !yes && (json || !io::stdin().is_terminal()) {
        return Err(CliError::SetupConsentRequired);
    }
    let context = Context::resolve()?;
    let status = service::status().await?;
    let staged = service::staged_dir(&context, None)?;
    let plan = build_plan(&context, paths, &status, &staged).await?;
    if !json {
        print!("{}", render_plan(&plan));
    }
    if !yes && !confirm()? {
        return Err(CliError::SetupDeclined);
    }
    let report = apply(&context, paths, plan).await?;
    if json {
        print!("{}", crate::commands::render_json(&report)?);
    } else {
        print!("{}", render_report(&report));
    }
    Ok(())
}

/// Reads the service state and the release plan without changing anything.
///
/// # Errors
///
/// Returns [`CliError::SetupBlocked`] for a refused plan or a service of
/// another version, and the errors of the reads otherwise.
async fn build_plan(
    context: &Context,
    paths: &Paths,
    status: &StatusReport,
    staged: &Path,
) -> Result<Plan, CliError> {
    let (service, dir, search_path) = if status.installed {
        match status.active_version.as_deref() {
            Some(VERSION) => {
                let dir = installed_version_dir(status)?;
                let search = installed_search_path(context).await?;
                (ServiceStep::Present, dir, search)
            }
            other => {
                return Err(CliError::SetupBlocked {
                    detail: format!(
                        "the installed service runs version {}, this pohunek is {VERSION}; \
                         run `pohunek service upgrade` first, setup never upgrades",
                        other.unwrap_or("unknown")
                    ),
                });
            }
        }
    } else {
        // The search path a fresh install records, so the plan judges the
        // packages against the PATH the daemon job will get.
        let (search, _report) = context.install_search_path()?;
        (
            ServiceStep::Install {
                from: staged.display().to_string(),
            },
            staged.to_path_buf(),
            search.to_env_value(),
        )
    };
    let report = DaemonRuntimes::default()
        .plan_detected(context, &dir, &search_path)
        .await?;
    if !report.refusals.is_empty() {
        return Err(CliError::SetupBlocked {
            detail: refusal_text(&report.refusals),
        });
    }
    let config_files = config_files(paths)
        .into_iter()
        .map(|(path, _default)| {
            let exists = path.try_exists()?;
            Ok((path, exists))
        })
        .collect::<Result<Vec<_>, CliError>>()?;
    Ok(Plan {
        service,
        dir,
        search_path,
        report,
        config_files,
    })
}

/// Applies a previewed plan, step by step, and runs the final checks.
///
/// # Errors
///
/// Returns [`CliError::SetupIncomplete`] when a step fails or the verdict is
/// not met, and the errors of the service and daemon calls.
async fn apply(context: &Context, paths: &Paths, plan: Plan) -> Result<SetupReport, CliError> {
    let Plan {
        service,
        dir,
        search_path,
        report: planned,
        ..
    } = plan;
    let mut applied = Vec::new();
    let service_step = service.clone();
    let (dir, search_path) = match service {
        ServiceStep::Install { .. } => install_service(context, &planned, &mut applied).await?,
        ServiceStep::Present => (dir, search_path),
    };

    let mut client = Client::connect(LOCAL_HOST, paths).await?;
    apply_packages_and_profiles(&mut client, &dir, &planned, &mut applied).await?;

    let env = IntegrationEnv {
        paths: Paths::resolve()?,
        home: context.home().map(Path::to_path_buf),
        state_dir: context.paths().state_dir.clone(),
    };
    let hermes = planned
        .integrations
        .iter()
        .any(|integration| integration.runtime == HERMES_RUNTIME);
    let others: Vec<IntegrationAction> = planned
        .integrations
        .iter()
        .filter(|integration| integration.runtime != HERMES_RUNTIME)
        .cloned()
        .collect();
    let hooks = reinstall_after_upgrade(&env, &others, hermes).await;
    for hook in &hooks {
        applied.push(format!("hook {}: {}", hook.integration, hook.status));
    }

    let config = install_config(paths, false)?;

    let after = DaemonRuntimes::default()
        .plan_detected(context, &dir, &search_path)
        .await?;
    let verdict = verdict(paths, &after, &hooks).await?;
    Ok(SetupReport {
        service: service_step,
        applied,
        packages: planned.packages,
        profiles: planned.profiles,
        integrations: planned.integrations,
        gaps: planned.gaps,
        hooks,
        config,
        verdict,
    })
}

/// Installs this release as the service, then checks that the installed
/// release plans what the preview showed.
///
/// Returns the installed version directory and the `PATH` of its daemon job.
///
/// # Errors
///
/// Returns [`CliError::SetupIncomplete`] when the installed plan differs from
/// the preview, and the errors of the install and the plan.
async fn install_service(
    context: &Context,
    planned: &EnsureReport,
    applied: &mut Vec<String>,
) -> Result<(PathBuf, String), CliError> {
    let installed = service::install(None, None).await?;
    applied.push(format!("service: installed version {VERSION}"));
    let version_dir = installed
        .daemon_executable
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| CliError::SetupIncomplete {
            detail: "the installed daemon has no version directory".to_owned(),
        })?;
    let installed_search = installed_search_path(context).await?;
    let after = DaemonRuntimes::default()
        .plan_detected(context, &version_dir, &installed_search)
        .await?;
    if after.packages != planned.packages
        || after.profiles != planned.profiles
        || after.integrations != planned.integrations
        || after.gaps != planned.gaps
        || !after.refusals.is_empty()
    {
        return Err(CliError::SetupIncomplete {
            detail: "the installed release plan differs from the preview; the service is \
                     installed, and no package, profile, hook or config was changed"
                .to_owned(),
        });
    }
    Ok((version_dir, installed_search))
}

/// Installs the planned packages and pins the planned profiles over the
/// daemon's RPCs, recording one line per outcome.
///
/// # Errors
///
/// Returns the errors of the daemon calls, and [`CliError::SetupIncomplete`]
/// for a planned digest or path the request cannot carry.
async fn apply_packages_and_profiles(
    client: &mut Client,
    dir: &Path,
    planned: &EnsureReport,
    applied: &mut Vec<String>,
) -> Result<(), CliError> {
    let release_dir = dir.join(RELEASE_RUNTIME_DIR);
    for package in planned
        .packages
        .iter()
        .filter(|package| package.status == PackageStatus::Install)
    {
        let archive = release_dir
            .join(RELEASE_PACKAGES_DIR)
            .join(format!("{}{RELEASE_ARCHIVE_SUFFIX}", package.runtime));
        let result = client
            .call::<method::PackageInstall>(PackageInstallParams {
                archive_path: path_text(&archive)?,
                trust: PackageTrust::Catalog {
                    catalog_path: path_text(&release_dir.join(RELEASE_CATALOG_FILE))?,
                },
                enable: true,
                select: true,
                dry_run: false,
            })
            .await?;
        applied.push(format!("package {}: {:?}", package.runtime, result.status));
    }
    for profile in planned
        .profiles
        .iter()
        .filter(|profile| profile.status == ProfileStatus::Bind)
    {
        let digest = PackageDigest::parse(&profile.digest).map_err(|_malformed| {
            CliError::SetupIncomplete {
                detail: format!("profile {}: its planned digest is not valid", profile.name),
            }
        })?;
        let bound = client
            .call::<method::PackageBindProfile>(PackageBindProfileParams {
                profile: profile.name.clone(),
                digest: Some(digest),
                dry_run: false,
            })
            .await?;
        applied.push(format!("profile {}: {:?}", profile.name, bound.status));
    }
    Ok(())
}

/// Runs the final checks and fails when any is not met.
///
/// # Errors
///
/// Returns [`CliError::SetupIncomplete`] naming every check that is not met.
async fn verdict(
    paths: &Paths,
    after: &EnsureReport,
    hooks: &[IntegrationReinstall],
) -> Result<Verdict, CliError> {
    let packages_missing: Vec<String> = after
        .packages
        .iter()
        .filter(|package| package.status != PackageStatus::Present)
        .map(|package| package.runtime.clone())
        .collect();
    let hooks_pending: Vec<String> = hooks
        .iter()
        .filter(|hook| hook.status != HOOK_INSTALLED)
        .map(|hook| match &hook.detail {
            Some(detail) => format!("{}: {detail}", hook.integration),
            None => hook.integration.clone(),
        })
        .collect();
    let health =
        health::fetch(LOCAL_HOST, paths)
            .await
            .map_err(|error| CliError::SetupIncomplete {
                detail: format!("the daemon does not answer its health check: {error}"),
            })?;
    let diagnosis = doctor::diagnose(paths).await?;
    let verdict = Verdict {
        packages_missing,
        hooks_pending,
        health: health.status,
        doctor: diagnosis.overall.as_str().to_owned(),
    };
    let mut problems = Vec::new();
    if !verdict.packages_missing.is_empty() {
        problems.push(format!(
            "packages not present: {}",
            verdict.packages_missing.join(", ")
        ));
    }
    if !verdict.hooks_pending.is_empty() {
        problems.push(format!(
            "hooks pending: {}",
            verdict.hooks_pending.join("; ")
        ));
    }
    if diagnosis.overall == DoctorStatus::Fail {
        problems.push("doctor reports a failed check; run `pohunek doctor`".to_owned());
    }
    if problems.is_empty() {
        Ok(verdict)
    } else {
        Err(CliError::SetupIncomplete {
            detail: problems.join("; "),
        })
    }
}

/// The version directory of the installed release.
///
/// # Errors
///
/// Returns [`CliError::SetupBlocked`] when the recorded prefix has no version
/// directory for this release.
fn installed_version_dir(status: &StatusReport) -> Result<PathBuf, CliError> {
    let prefix = status
        .prefix
        .as_deref()
        .ok_or_else(|| CliError::SetupBlocked {
            detail: "the installed service records no prefix".to_owned(),
        })?;
    install_layout(prefix)?
        .version_dir(VERSION)
        .ok_or_else(|| CliError::SetupBlocked {
            detail: format!("the installed prefix has no directory for version {VERSION}"),
        })
}

/// Asks once whether to apply the plan.
///
/// # Errors
///
/// Returns [`CliError::Io`] when the answer cannot be read.
fn confirm() -> Result<bool, CliError> {
    let mut stderr = io::stderr();
    write!(stderr, "{CONFIRM_PROMPT}")?;
    stderr.flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let answer = answer.trim();
    Ok(CONFIRM_ANSWERS
        .iter()
        .any(|expected| answer.eq_ignore_ascii_case(expected)))
}

/// A path as the daemon request carries it.
///
/// # Errors
///
/// Returns [`CliError::SetupIncomplete`] for a path that is not valid UTF-8.
fn path_text(path: &Path) -> Result<String, CliError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| CliError::SetupIncomplete {
            detail: format!(
                "{} is not valid UTF-8, which the daemon request cannot carry",
                path.display()
            ),
        })
}

/// The refusals of a plan, one per line.
fn refusal_text(refusals: &[Refusal]) -> String {
    refusals
        .iter()
        .map(|refusal| format!("{}: {}", refusal.subject, refusal.detail))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Renders the preview: every effect and gap, before anything is written.
fn render_plan(plan: &Plan) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "pohunek setup, version {VERSION}");
    match &plan.service {
        ServiceStep::Install { from } => {
            let _ = writeln!(out, "service: install this release from {from}");
        }
        ServiceStep::Present => {
            let _ = writeln!(out, "service: runs version {VERSION}; kept");
        }
    }
    let _ = writeln!(out, "packages:");
    for package in &plan.report.packages {
        let _ = writeln!(
            out,
            "  {}  {} {}  {:?}  reasons {:?}",
            package.runtime, package.package, package.version, package.status, package.reasons
        );
    }
    let _ = writeln!(out, "profiles:");
    for profile in &plan.report.profiles {
        let _ = writeln!(
            out,
            "  {} (base {}): {:?}",
            profile.name, profile.base, profile.status
        );
    }
    let _ = writeln!(out, "hooks:");
    for integration in &plan.report.integrations {
        match &integration.profile {
            Some(profile) => {
                let _ = writeln!(out, "  {} profile {profile}", integration.runtime);
            }
            None => {
                let _ = writeln!(out, "  {}", integration.runtime);
            }
        }
    }
    let _ = writeln!(out, "config:");
    for (path, exists) in &plan.config_files {
        let state = if *exists { "keep (exists)" } else { "create" };
        let _ = writeln!(out, "  {state}: {}", path.display());
    }
    let _ = writeln!(out, "gaps:");
    for gap in &plan.report.gaps {
        let _ = writeln!(out, "  {}: {} ({})", gap.runtime, gap.detail, gap.code);
    }
    out
}

/// Renders the outcome of a completed run.
fn render_report(report: &SetupReport) -> String {
    let mut out = String::new();
    for line in &report.applied {
        let _ = writeln!(out, "{line}");
    }
    for path in &report.config.created {
        let _ = writeln!(out, "created: {path}");
    }
    for path in &report.config.skipped {
        let _ = writeln!(out, "skipped (exists): {path}");
    }
    let _ = writeln!(
        out,
        "verdict: health {}, doctor {}",
        report.verdict.health, report.verdict.doctor
    );
    out
}
