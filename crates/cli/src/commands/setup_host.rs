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
//! directory. A difference stops the run after the service install and before
//! any package, profile, hook or config write. The installed service is kept,
//! and a re-run plans from it, so the stop is a state to converge from.
//!
//! Hook gaps (a Hermes home without the pohunek marker) never fail the run:
//! setup does not choose a Hermes access policy for the owner, so such a home
//! is reported with the command that installs it, and nothing is written there.

// Rust guideline compliant 2026-10-14

use std::fmt::{self, Write as _};
use std::future::Future;
use std::io::{self, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::pin::Pin;

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
use crate::commands::integration::{hermes_home_gaps, reinstall_after_upgrade, IntegrationEnv};
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

/// Hook status of a home that setup attempted and could not install.
const HOOK_FAILED: &str = "failed";

/// Hook status of a home setup leaves alone: a gap with its install command.
const HOOK_GAP: &str = "gap";

/// Answers, compared case-insensitively, that confirm the plan at the prompt.
const CONFIRM_ANSWERS: [&str; 2] = ["y", "yes"];

/// Question asked once after the preview, on the terminal's error stream.
const CONFIRM_PROMPT: &str = "Apply this plan? [y/N] ";

/// Reads the release planner's report for a directory: the release's
/// `pohunekd ensure-official-runtimes --dry-run --detect`.
pub trait ReleasePlanner: fmt::Debug + Send + Sync {
    /// Returns the report of the planner for the release in `dir`, run with
    /// the `PATH` `search_path` gives the daemon job. It changes nothing.
    fn plan<'a>(
        &'a self,
        context: &'a Context,
        dir: &'a Path,
        search_path: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<EnsureReport, service::Error>> + Send + 'a>>;
}

impl ReleasePlanner for DaemonRuntimes {
    fn plan<'a>(
        &'a self,
        context: &'a Context,
        dir: &'a Path,
        search_path: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<EnsureReport, service::Error>> + Send + 'a>> {
        Box::pin(self.plan_detected(context, dir, search_path))
    }
}

/// The service step of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ServiceStep {
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
pub struct Verdict {
    /// Runtimes whose previewed package is not present after the apply.
    pub packages_missing: Vec<String>,
    /// Hook homes setup attempted and could not install.
    pub hooks_failed: Vec<String>,
    /// Hook homes left as gaps, each with the command that installs it.
    pub hook_gaps: Vec<String>,
    /// The daemon's health status.
    pub health: String,
    /// The overall status of `doctor`.
    pub doctor: String,
}

/// What a completed run did and found.
#[derive(Debug, Serialize)]
pub struct SetupReport {
    /// The service step that ran.
    pub service: ServiceStep,
    /// One line per change made: the package, profile and hook outcomes.
    pub applied: Vec<String>,
    /// The previewed packages, as previewed.
    pub packages: Vec<PackageAction>,
    /// The previewed profile pins, as previewed.
    pub profiles: Vec<ProfileAction>,
    /// The previewed integrations, as previewed.
    pub integrations: Vec<IntegrationAction>,
    /// The daemon's gaps, as previewed.
    pub gaps: Vec<Gap>,
    /// The outcome of every integration hook setup attempted.
    pub hooks: Vec<IntegrationReinstall>,
    /// The config files written and kept.
    pub config: ConfigResult,
    /// The final checks.
    pub verdict: Verdict,
}

/// The read-only plan of a run, before anything is written.
#[derive(Debug)]
pub struct HostPlan {
    /// The service step the plan assumes.
    pub service: ServiceStep,
    /// The release directory the plan was read from.
    pub dir: PathBuf,
    /// The `PATH` the daemon job runs with.
    pub search_path: String,
    /// What the release daemon reported for `dir`.
    pub report: EnsureReport,
    /// Each config file with whether it already exists.
    pub config_files: Vec<(PathBuf, bool)>,
    /// The hook homes setup leaves as gaps, known before anything is written.
    pub hook_gaps: Vec<IntegrationReinstall>,
}

/// Why a host setup step stopped, with its stable code and next step.
///
/// The code and the messages are the ones `pohunek setup` prints.
#[derive(Debug)]
pub struct SetupError(CliError);

impl SetupError {
    /// The stable code of the failure, as the `--json` error document carries it.
    #[must_use]
    pub fn code(&self) -> String {
        self.0.to_protocol_error().code
    }

    /// The next step the failure names, when one applies.
    #[must_use]
    pub fn recover(&self) -> Option<String> {
        self.0.recover_hint()
    }
}

impl fmt::Display for SetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for SetupError {}

/// Reads the plan of a host setup for the installation `context` describes,
/// with `staged` as the release directory of a fresh install.
///
/// # Errors
///
/// Returns [`SetupError`] for a refused plan, a service of another version,
/// or a plan that cannot be read. Nothing is written.
pub async fn plan(
    context: &Context,
    staged: &Path,
    planner: &dyn ReleasePlanner,
) -> Result<HostPlan, SetupError> {
    plan_in(context, staged, planner).await.map_err(SetupError)
}

/// Applies a plan read by [`plan`], then runs the final checks.
///
/// # Errors
///
/// Returns [`SetupError`] when a step fails or the verdict is not met. A
/// failure after the first change keeps the finished steps.
pub async fn apply(
    context: &Context,
    plan: HostPlan,
    planner: &dyn ReleasePlanner,
) -> Result<SetupReport, SetupError> {
    apply_in(context, plan, planner).await.map_err(SetupError)
}

/// Run bare `pohunek setup`.
///
/// # Errors
///
/// Returns [`CliError::SetupConsentRequired`] without `--yes` when no
/// confirmation can be asked, [`CliError::SetupDeclined`] when the owner says
/// no, [`CliError::SetupBlocked`] before any change, and
/// [`CliError::SetupIncomplete`] when a step after the first change fails.
pub(crate) async fn run(yes: bool, json: bool) -> Result<(), CliError> {
    if !yes && (json || !io::stdin().is_terminal()) {
        return Err(CliError::SetupConsentRequired);
    }
    let context = Context::resolve()?;
    let staged = service::staged_dir(&context, None)?;
    let planner = DaemonRuntimes::default();
    let plan = plan_in(&context, &staged, &planner).await?;
    if !json {
        print!("{}", render_plan(&plan));
    }
    if !yes && !confirm()? {
        return Err(CliError::SetupDeclined);
    }
    let report = apply_in(&context, plan, &planner).await?;
    if json {
        print!("{}", crate::commands::render_json(&report)?);
    } else {
        print!("{}", render_report(&report));
    }
    Ok(())
}

/// The paths of the installation `context` describes.
fn paths_of(context: &Context) -> Paths {
    Paths::from_base(context.paths())
}

/// The integration environment of the installation `context` describes.
fn integration_env(context: &Context) -> IntegrationEnv {
    IntegrationEnv {
        paths: paths_of(context),
        home: context.home().map(Path::to_path_buf),
        state_dir: context.paths().state_dir.clone(),
    }
}

/// Reads the service state and the release plan without changing anything.
///
/// # Errors
///
/// Returns [`CliError::SetupBlocked`] for a refused plan or a service of
/// another version, and the errors of the reads otherwise.
async fn plan_in(
    context: &Context,
    staged: &Path,
    planner: &dyn ReleasePlanner,
) -> Result<HostPlan, CliError> {
    let status = service::status_in(context).await?;
    let (service, dir, search_path) = if status.installed {
        match status.active_version.as_deref() {
            Some(VERSION) => {
                let dir = installed_version_dir(&status)?;
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
    let report = planner.plan(context, &dir, &search_path).await?;
    if !report.refusals.is_empty() {
        return Err(CliError::SetupBlocked {
            detail: refusal_text(&report.refusals),
        });
    }
    let config_files = config_files(&paths_of(context))
        .into_iter()
        .map(|(path, _default)| {
            let exists = path.try_exists()?;
            Ok((path, exists))
        })
        .collect::<Result<Vec<_>, CliError>>()?;
    let hook_gaps = if report
        .integrations
        .iter()
        .any(|integration| integration.runtime == HERMES_RUNTIME)
    {
        hermes_home_gaps(&integration_env(context))
    } else {
        Vec::new()
    };
    Ok(HostPlan {
        service,
        dir,
        search_path,
        report,
        config_files,
        hook_gaps,
    })
}

/// Applies a previewed plan, step by step, and runs the final checks.
///
/// # Errors
///
/// Returns [`CliError::SetupIncomplete`] when a step fails or the verdict is
/// not met, and the errors of the service and daemon calls.
async fn apply_in(
    context: &Context,
    plan: HostPlan,
    planner: &dyn ReleasePlanner,
) -> Result<SetupReport, CliError> {
    let HostPlan {
        service,
        dir,
        search_path,
        report: previewed,
        ..
    } = plan;
    let mut applied = Vec::new();
    let service_step = service.clone();
    let (dir, search_path) = match service {
        ServiceStep::Install { from } => {
            install_service(
                context,
                &PathBuf::from(from),
                &previewed,
                &mut applied,
                planner,
            )
            .await?
        }
        ServiceStep::Present => (dir, search_path),
    };

    let paths = paths_of(context);
    let mut client = Client::connect(LOCAL_HOST, &paths).await?;
    apply_packages_and_profiles(&mut client, &dir, &previewed, &mut applied).await?;

    let env = integration_env(context);
    let hermes = previewed
        .integrations
        .iter()
        .any(|integration| integration.runtime == HERMES_RUNTIME);
    let others: Vec<IntegrationAction> = previewed
        .integrations
        .iter()
        .filter(|integration| integration.runtime != HERMES_RUNTIME)
        .cloned()
        .collect();
    let hooks = reinstall_after_upgrade(&env, &others, hermes).await;
    for hook in &hooks {
        applied.push(format!("hook {}: {}", hook.integration, hook.status));
    }

    let config = install_config(&paths, false)?;

    let after = planner.plan(context, &dir, &search_path).await?;
    let verdict = verdict(&paths, &after, &hooks).await?;
    Ok(SetupReport {
        service: service_step,
        applied,
        packages: previewed.packages,
        profiles: previewed.profiles,
        integrations: previewed.integrations,
        gaps: previewed.gaps,
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
/// Returns [`CliError::SetupInstalledOnly`] when the installed plan differs
/// from the preview, and the errors of the install and the plan.
async fn install_service(
    context: &Context,
    from: &Path,
    previewed: &EnsureReport,
    applied: &mut Vec<String>,
    planner: &dyn ReleasePlanner,
) -> Result<(PathBuf, String), CliError> {
    let installed = service::install_in(context, Some(from.to_path_buf()), None).await?;
    applied.push(format!("service: installed version {}", installed.version));
    let version_dir = installed
        .daemon_executable
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| CliError::SetupIncomplete {
            detail: "the installed daemon has no version directory".to_owned(),
        })?;
    let installed_search = installed_search_path(context).await?;
    let after = planner
        .plan(context, &version_dir, &installed_search)
        .await?;
    if after.packages != previewed.packages
        || after.profiles != previewed.profiles
        || after.integrations != previewed.integrations
        || after.gaps != previewed.gaps
        || !after.refusals.is_empty()
    {
        return Err(CliError::SetupInstalledOnly {
            version: installed.version,
            prefix: installed.prefix,
            detail: "the release plan read after the install differs from the preview".to_owned(),
        });
    }
    Ok((version_dir, installed_search))
}

/// Installs the previewed packages and pins the previewed profiles over the
/// daemon's RPCs, recording one line per outcome.
///
/// # Errors
///
/// Returns the errors of the daemon calls, and [`CliError::SetupIncomplete`]
/// for a previewed digest or path the request cannot carry.
async fn apply_packages_and_profiles(
    client: &mut Client,
    dir: &Path,
    previewed: &EnsureReport,
    applied: &mut Vec<String>,
) -> Result<(), CliError> {
    let release_dir = dir.join(RELEASE_RUNTIME_DIR);
    for package in previewed
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
        applied.push(format!(
            "package {}: {}",
            package.runtime,
            wire_name(&result.status)
        ));
    }
    for profile in previewed
        .profiles
        .iter()
        .filter(|profile| profile.status == ProfileStatus::Bind)
    {
        let digest = PackageDigest::parse(&profile.digest).map_err(|_malformed| {
            CliError::SetupIncomplete {
                detail: format!(
                    "profile {}: its previewed digest is not valid",
                    profile.name
                ),
            }
        })?;
        let bound = client
            .call::<method::PackageBindProfile>(PackageBindProfileParams {
                profile: profile.name.clone(),
                digest: Some(digest),
                dry_run: false,
            })
            .await?;
        applied.push(format!(
            "profile {}: {}",
            profile.name,
            wire_name(&bound.status)
        ));
    }
    Ok(())
}

/// Runs the final checks and fails when any is not met.
///
/// Hook gaps are reported and do not fail the verdict.
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
    let hooks_failed = hook_lines(hooks, HOOK_FAILED);
    let hook_gaps = hook_lines(hooks, HOOK_GAP);
    let health =
        health::fetch(LOCAL_HOST, paths)
            .await
            .map_err(|error| CliError::SetupIncomplete {
                detail: format!("the daemon does not answer its health check: {error}"),
            })?;
    let diagnosis = doctor::diagnose(paths).await?;
    let verdict = Verdict {
        packages_missing,
        hooks_failed,
        hook_gaps,
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
    if !verdict.hooks_failed.is_empty() {
        problems.push(format!("hooks failed: {}", verdict.hooks_failed.join("; ")));
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

/// The hook lines of `hooks` with `status`, as `integration: detail`.
fn hook_lines(hooks: &[IntegrationReinstall], status: &str) -> Vec<String> {
    hooks
        .iter()
        .filter(|hook| hook.status == status)
        .map(|hook| match &hook.detail {
            Some(detail) => format!("{}: {detail}", hook.integration),
            None => hook.integration.clone(),
        })
        .collect()
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

/// The name a value has on the wire, which the JSON output and the human
/// output both use (`install`, `present`, `detected`, ...).
fn wire_name<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
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
fn render_plan(plan: &HostPlan) -> String {
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
            "  {}  {} {}  {}  reasons {}",
            package.runtime,
            package.package,
            package.version,
            wire_name(&package.status),
            package
                .reasons
                .iter()
                .map(wire_name)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let _ = writeln!(out, "profiles:");
    for profile in &plan.report.profiles {
        let _ = writeln!(
            out,
            "  {} (base {}): {}",
            profile.name,
            profile.base,
            wire_name(&profile.status)
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
    let _ = writeln!(out, "hook gaps:");
    for gap in &plan.hook_gaps {
        let _ = writeln!(
            out,
            "  {}: {}",
            gap.integration,
            gap.detail.as_deref().unwrap_or_default()
        );
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
    for line in &report.verdict.hook_gaps {
        let _ = writeln!(out, "hook gap: {line}");
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
