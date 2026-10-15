//! `pohunek setup` — the one command that makes this host ready to run pohunek.
//!
//! The run has six steps, in this order:
//!
//! 1. Service: without an installed service the release binaries next to the
//!    running `pohunek` are installed. The install engine runs the release's
//!    runtime step itself, for the runtimes its profiles, sessions and
//!    integrations use, and reports what it applied. The same version is kept
//!    as it is. A different installed version is a gap; setup never upgrades.
//! 2. Plan: the release's `pohunekd ensure-official-runtimes --dry-run --detect`
//!    reads the catalog and reports the packages, profiles, integrations and
//!    gaps. It only reads, so it runs before the preview and before any write.
//! 3. Preview and consent: every effect and gap is shown first. Without a
//!    terminal and without `--yes` the run is refused. `--json --yes` applies
//!    and prints one result document that carries the same preview; `--json`
//!    alone is refused, because it cannot ask.
//! 4. Apply: after a fresh install the plan is read again and must match the
//!    preview, modulo exactly the transitions the install engine performs.
//!    Setup then applies what remains over the daemon's RPCs, installs the
//!    hooks, and writes the missing config files.
//! 5. Config: the non-forcing `setup config` writer; existing files are kept.
//! 6. Verdict: a third plan must carry every previewed runtime as present,
//!    no refusal, and no pending profile pin; the hooks and `doctor` must pass.
//!
//! A failure after the first change keeps the finished steps and names them.
//! A stop after the service install names the install's own changes. Hermes
//! homes without the pohunek marker are gaps: setup does not choose a Hermes
//! access policy for the owner and writes nothing into them.

// Rust guideline compliant 2026-10-14

use std::ffi::OsStr;
use std::fmt::{self, Write as _};
use std::fs;
use std::future::Future;
use std::io::{self, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::pin::Pin;

use pohunek_service_config::runtimes::{
    CatalogStatus, EnsureReport, Gap, IntegrationAction, PackageAction, PackageStatus,
    ProfileAction, ProfileStatus, Refusal, CODE_PACKAGE_UNAVAILABLE_FOR_PLATFORM,
    RELEASE_RUNTIME_DIR,
};
use protocol::{
    method, DoctorStatus, IntegrationInstallState, IntegrationRecovery, IntegrationStatusParams,
    PackageBindProfileParams, PackageDigest, PackageInstallParams, PackageSelectParams,
    PackageSetEnabledParams, PackageTrust, RuntimeRef,
};
use serde::Serialize;

use crate::client::Client;
use crate::commands::doctor;
use crate::commands::health;
use crate::commands::integration::{
    hermes_home_plan, reinstall_after_upgrade, reinstall_previewed_hermes_homes, IntegrationEnv,
    HERMES_CURRENT, HERMES_UPDATE,
};
use crate::commands::setup::{config_files, install_config_into, ConfigResult};
use crate::error::CliError;
use crate::hermes_integration::runner::safe_ancestor;
use crate::paths::Paths;
use crate::service::engine::{install_layout, installed_job_path, recorded_job_path};
use crate::service::report::IntegrationReinstall;
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

/// Manifest at the root of a sealed release archive (`packaging/write-manifest`).
const ARCHIVE_MANIFEST: &str = "MANIFEST";

/// The archive's directory of installer scripts, and its own verifier, which
/// its installer `packaging/install-daemon.sh` runs before anything from the
/// archive.
const ARCHIVE_PACKAGING_DIR: &str = "packaging";
const ARCHIVE_VERIFIER: &str = "packaging/verify-archive";

/// Component and binaries the daemon archive must carry, as
/// `packaging/install-daemon.sh` passes them to the verifier.
const DAEMON_ARCHIVE_COMPONENT: &str = "daemon";
const DAEMON_ARCHIVE_BINARIES: [&str; 3] = ["pohunek", "pohunekd", "pohunek-sessiond"];

/// The search path the verifier runs with: the system's own tools, so a
/// directory of the caller's `PATH` cannot stand in for `uname`, `find` or the
/// digest tool it relies on.
const VERIFIER_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// The superuser, the one other account a release directory may belong to.
const ROOT_UID: u32 = 0;

/// Suffix of a release package archive, named after the runtime id.
///
/// Matches `ARCHIVE_SUFFIX` of the release daemon's `ensure-official-runtimes`.
const RELEASE_ARCHIVE_SUFFIX: &str = ".tar.zst";

/// Runtime id whose Hermes homes the CLI reinstalls itself, offline.
const HERMES_RUNTIME: &str = "hermes";

/// Directory of the default Hermes home below the user's home directory.
const HERMES_HOME_DIR: &str = ".hermes";

/// Hook status of a home setup attempted and could not install.
const HOOK_FAILED: &str = "failed";

/// Hook status of a home setup leaves alone: a gap with its install command.
const HOOK_GAP: &str = "gap";

/// Answers, compared case-insensitively, that confirm the plan at the prompt.
const CONFIRM_ANSWERS: [&str; 2] = ["y", "yes"];

/// Question asked once after the preview, on the terminal's error stream.
const CONFIRM_PROMPT: &str = "Apply this plan? [y/N] ";

/// Package status the install engine leaves a package in once it installed it.
const ENGINE_INSTALLED: &str = "installed";

/// Gap codes that the install engine's runtime step, which runs without
/// detection, reports as refusals for every runtime the plan lists them for.
///
/// Detection only adds the runtimes the catalog lists for this platform, so a
/// runtime without a package here is one the host uses. The same gap also
/// names a runtime the release catalog lists more than once for this
/// platform; setup refuses that release even where the engine would not plan
/// the runtime. A runtime served by another package can be a detected one the
/// engine never plans, so that gap is not here.
const ENGINE_REFUSED_GAPS: [&str; 1] = [CODE_PACKAGE_UNAVAILABLE_FOR_PLATFORM];

/// Profile status the install engine leaves a profile in once it pinned it.
const ENGINE_BOUND: &str = "bound";

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

/// One config file the run writes or keeps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigPreview {
    /// The file's path.
    pub path: String,
    /// Whether the file already exists, in which case the run keeps it.
    pub exists: bool,
}

/// Everything a run will do, as the owner sees it before any write.
///
/// The Hermes homes are every home the apply acts on: a `update` for a home
/// with the pohunek marker, a `gap` with its install command for a home
/// without it, and `failed` for a home that cannot be planned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Preview {
    /// The service step.
    pub service: ServiceStep,
    /// The planned packages.
    pub packages: Vec<PackageAction>,
    /// The planned profile pins.
    pub profiles: Vec<ProfileAction>,
    /// The planned integrations.
    pub integrations: Vec<IntegrationAction>,
    /// The daemon's gaps.
    pub gaps: Vec<Gap>,
    /// The release catalog's trust state: `record` means the install or the
    /// apply records it.
    pub catalog: CatalogStatus,
    /// What the catalog state means for this run when no step records it.
    pub catalog_note: Option<String>,
    /// Every Hermes home the apply acts on, with its planned state.
    pub hermes_homes: Vec<IntegrationReinstall>,
    /// The config files, each with whether it exists.
    pub config: Vec<ConfigPreview>,
}

/// The final checks of a run.
#[derive(Debug, Serialize)]
pub struct Verdict {
    /// Previewed runtimes that are not present in the plan read after the apply.
    pub packages_missing: Vec<String>,
    /// Runtimes the plan read after the apply lists but the preview did not.
    pub packages_unexpected: Vec<String>,
    /// Profiles the preview pins that the plan read after the apply still lists.
    pub profiles_pending: Vec<String>,
    /// Refusals of the plan read after the apply.
    pub refusals: Vec<String>,
    /// Hook homes setup attempted and could not install.
    pub hooks_failed: Vec<String>,
    /// Hook homes left as gaps, each with the command that installs it.
    pub hook_gaps: Vec<String>,
    /// The release catalog's trust state in the plan read after the apply.
    pub catalog: String,
    /// The daemon's health status.
    pub health: String,
    /// The overall status of `doctor`.
    pub doctor: String,
}

/// What a completed run did and found.
#[derive(Debug, Serialize)]
pub struct SetupReport {
    /// The preview the owner was shown, in full.
    pub preview: Preview,
    /// One line per change made, in order.
    pub applied: Vec<String>,
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
    service: ServiceStep,
    /// The release directory the plan was read from.
    dir: PathBuf,
    /// The `PATH` the daemon job runs with.
    search_path: String,
    /// What the release planner reported for `dir`.
    previewed: EnsureReport,
    /// The preview shown to the owner.
    pub preview: Preview,
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
/// failure after the first change names the changes already made.
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
/// With `dry_run` it only prints the plan. Otherwise it returns
/// [`CliError::SetupConsentRequired`] without `--yes` when no
/// confirmation can be asked, [`CliError::SetupDeclined`] when the owner says
/// no, [`CliError::SetupBlocked`] before any change, and
/// [`CliError::SetupIncomplete`] or [`CliError::SetupInstalledOnly`] when a
/// step after the first change fails.
pub(crate) async fn run(yes: bool, json: bool, dry_run: bool) -> Result<(), CliError> {
    if dry_run {
        return preview_only(json).await;
    }
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

/// Run `pohunek setup --dry-run`: reads the plan the run would preview and
/// prints it, the preview document with `--json`, and changes nothing.
///
/// # Errors
///
/// Returns the errors of reading the plan, as a run returns them before its
/// first change.
async fn preview_only(json: bool) -> Result<(), CliError> {
    let context = Context::resolve()?;
    let staged = service::staged_dir(&context, None)?;
    let planner = DaemonRuntimes::default();
    let plan = plan_in(&context, &staged, &planner).await?;
    if json {
        print!("{}", crate::commands::render_json(&plan.preview)?);
    } else {
        print!("{}", render_plan(&plan));
        println!("dry run: nothing was changed");
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

/// Whether the plan installs or keeps the official Hermes package.
fn serves_hermes(plan: &EnsureReport) -> bool {
    plan.packages
        .iter()
        .any(|package| package.runtime == HERMES_RUNTIME)
}

/// Stops a run that would install the service while the plan lists a gap that
/// the install engine's own runtime step refuses.
///
/// The engine applies the plan without detection. There, a runtime the host
/// uses (a profile, a session or an integration) without a package for this
/// platform is a refusal, and a refusal rolls the install back. The plan lists
/// it as a gap, so it stops the run here, before any change.
///
/// # Errors
///
/// Returns [`CliError::SetupBlocked`] naming every such gap.
fn refuse_install_gaps(plan: &EnsureReport) -> Result<(), CliError> {
    let refused: Vec<String> = plan
        .gaps
        .iter()
        .filter(|gap| ENGINE_REFUSED_GAPS.contains(&gap.code.as_str()))
        .map(|gap| format!("{}: {} ({})", gap.runtime, gap.detail, gap.code))
        .collect();
    if refused.is_empty() {
        return Ok(());
    }
    Err(CliError::SetupBlocked {
        detail: format!(
            "installing the service refuses what this plan lists as gaps: {}; remove or repin \
             the profile that uses the runtime, then re-run `pohunek setup`",
            refused.join("; ")
        ),
    })
}

/// Whether the plan installs or selects a Hermes package, which changes the
/// plugin every marked Hermes home is updated to.
fn changes_hermes_selection(plan: &EnsureReport) -> bool {
    plan.packages.iter().any(|package| {
        package.runtime == HERMES_RUNTIME
            && matches!(
                package.status,
                PackageStatus::Install | PackageStatus::Installed
            )
    })
}

/// Whether the default Hermes home, `$HOME/.hermes`, is a directory. Its
/// named profiles live below it, so without it there is no home to plan.
fn hermes_home_exists(env: &IntegrationEnv) -> bool {
    env.home.as_deref().is_some_and(|home| {
        fs::symlink_metadata(home.join(HERMES_HOME_DIR)).is_ok_and(|metadata| metadata.is_dir())
    })
}

/// The note for a catalog that is to be recorded but that no step of this
/// run records: an installed service, and no official package to install.
/// A fresh install records the catalog itself, through its runtime step.
fn catalog_note(service: &ServiceStep, previewed: &EnsureReport) -> Option<String> {
    let installs = previewed
        .packages
        .iter()
        .any(|package| package.status == PackageStatus::Install);
    (*service == ServiceStep::Present && previewed.catalog == CatalogStatus::Record && !installs)
        .then(|| {
            "the release catalog's trust state is recorded with the next official package install or `pohunek service upgrade`"
                .to_owned()
        })
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
    // Read without side effects, so the plan a dry run prints leaves the host
    // as it was.
    let status = service::installed_facts(context)?;
    if let Some(pending) = &status.pending_transaction {
        return Err(CliError::SetupBlocked {
            detail: format!(
                "an interrupted {} of version {} stopped after step {}; resume it by rerunning \
                 the interrupted `pohunek service {}`, or roll it back with `pohunek service uninstall`",
                pending.operation, pending.version, pending.step, pending.operation
            ),
        });
    }
    let (service, dir, search_path) = if status.installed {
        match status.active_version.as_deref() {
            Some(VERSION) => {
                let dir = installed_version_dir(&status)?;
                let search = installed_job_path(context).await?.value;
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
        // The plan runs the release's daemon, so a sealed archive is verified
        // before anything from it runs.
        verify_sealed_archive(staged, context.uid()).await?;
        // The PATH the daemon job of a fresh install gets: the search path the
        // install records, or the service manager's own when it records none,
        // so the plan judges the packages as the installed daemon will.
        let (search, _report) = context.install_search_path()?;
        let job = recorded_job_path(context, &search, None)
            .await
            .map_err(|error| CliError::SetupBlocked {
                detail: format!(
                    "the plan needs the PATH the user service manager gives the daemon \
                     job, and it cannot be read ({error}); run setup in a login session \
                     of this user, where the user service manager runs"
                ),
            })?;
        (
            ServiceStep::Install {
                from: staged.display().to_string(),
            },
            staged.to_path_buf(),
            job.value,
        )
    };
    let previewed = planner.plan(context, &dir, &search_path).await?;
    if !previewed.refusals.is_empty() {
        return Err(CliError::SetupBlocked {
            detail: refusal_text(&previewed.refusals),
        });
    }
    if matches!(service, ServiceStep::Install { .. }) {
        refuse_install_gaps(&previewed)?;
    }
    let config = config_files(&paths_of(context))
        .into_iter()
        .map(|(path, _default)| {
            let exists = path.try_exists()?;
            Ok(ConfigPreview {
                path: path.display().to_string(),
                exists,
            })
        })
        .collect::<Result<Vec<_>, CliError>>()?;
    // The release step lists no Hermes home: the Hermes package declares none,
    // and its plugin is installed by this CLI. So the homes are planned here
    // whenever the plan serves Hermes and its default home exists.
    let env = integration_env(context);
    let hermes_homes = if serves_hermes(&previewed) && hermes_home_exists(&env) {
        hermes_home_plan(
            &env,
            OsStr::new(&search_path),
            changes_hermes_selection(&previewed),
        )
    } else {
        Vec::new()
    };
    let preview = Preview {
        service: service.clone(),
        packages: previewed.packages.clone(),
        profiles: previewed.profiles.clone(),
        integrations: previewed.integrations.clone(),
        gaps: previewed.gaps.clone(),
        catalog: previewed.catalog,
        catalog_note: catalog_note(&service, &previewed),
        hermes_homes,
        config,
    };
    Ok(HostPlan {
        service,
        dir,
        search_path,
        previewed,
        preview,
    })
}

/// Applies a previewed plan, step by step, and runs the final checks.
///
/// Errors after the first change carry the changes made so far.
///
/// # Errors
///
/// Returns the errors of the steps, each with the changes made before it.
async fn apply_in(
    context: &Context,
    plan: HostPlan,
    planner: &dyn ReleasePlanner,
) -> Result<SetupReport, CliError> {
    let mut applied = Vec::new();
    let steps = apply_steps(context, &plan, &mut applied, planner).await;
    match steps {
        Ok((hooks, config, verdict)) => Ok(SetupReport {
            preview: plan.preview,
            applied,
            hooks,
            config,
            verdict,
        }),
        Err(error) => Err(with_changes(error, applied)),
    }
}

/// Attaches the changes made before `error` to it.
fn with_changes(error: CliError, applied: Vec<String>) -> CliError {
    match error {
        installed_only @ CliError::SetupInstalledOnly { .. } => installed_only,
        CliError::SetupIncomplete { detail, .. } => CliError::SetupIncomplete { detail, applied },
        error if applied.is_empty() => error,
        error => CliError::SetupIncomplete {
            detail: error.to_string(),
            applied,
        },
    }
}

/// The steps after the preview: the service, the remaining RPCs, the hooks,
/// the config and the verdict.
///
/// # Errors
///
/// Returns the error of the first step that fails.
async fn apply_steps(
    context: &Context,
    plan: &HostPlan,
    applied: &mut Vec<String>,
    planner: &dyn ReleasePlanner,
) -> Result<(Vec<IntegrationReinstall>, ConfigResult, Verdict), CliError> {
    let previewed = &plan.previewed;
    let hermes_homes = &plan.preview.hermes_homes;
    let (dir, search_path, remaining) = match &plan.service {
        ServiceStep::Install { from } => {
            install_service(context, Path::new(from), previewed, applied, planner).await?
        }
        ServiceStep::Present => {
            let current = recheck_present(context, plan, planner).await?;
            (plan.dir.clone(), plan.search_path.clone(), current)
        }
    };

    let paths = paths_of(context);
    let mut client = Client::connect(LOCAL_HOST, &paths).await?;
    let installs_packages = remaining
        .packages
        .iter()
        .any(|package| package.status == PackageStatus::Install);
    apply_remaining(&mut client, &dir, &remaining, applied).await?;

    let env = integration_env(context);
    let mut stale = Vec::new();
    for integration in &remaining.integrations {
        if integration.runtime != HERMES_RUNTIME && !hook_is_current(&mut client, integration).await
        {
            stale.push(integration.clone());
        }
    }
    let mut hooks = reinstall_after_upgrade(&env, &stale, false).await;
    if !hermes_homes.is_empty() {
        hooks.extend(reinstall_previewed_hermes_homes(
            &env,
            hermes_homes,
            OsStr::new(&search_path),
        ));
    }
    // A current Hermes home was left as it is, so it is not a change.
    for hook in hooks.iter().filter(|hook| hook.status != HERMES_CURRENT) {
        applied.push(format!("hook {}: {}", hook.integration, hook.status));
    }

    let mut config = ConfigResult::default();
    let written = install_config_into(&paths, false, &mut config);
    for path in &config.created {
        applied.push(format!("config created: {path}"));
    }
    written?;

    let after = planner.plan(context, &dir, &search_path).await?;
    let catalog_recorded = installs_packages && previewed.catalog == CatalogStatus::Record;
    let verdict = verdict(&paths, previewed, &after, &hooks, catalog_recorded).await?;
    Ok((hooks, config, verdict))
}

/// Whether the daemon reports the hook of `action` as current in its home, so
/// that installing it again would change nothing. A report that cannot be
/// read, or that ignores the profile selector, is not current: the hook is
/// then installed, and the install reports any failure.
async fn hook_is_current(client: &mut Client, action: &IntegrationAction) -> bool {
    let params = IntegrationStatusParams {
        agent: Some(RuntimeRef::from_wire(&action.runtime)),
        profile: action.profile.clone(),
        all_profiles: false,
    };
    client
        .call::<method::IntegrationStatus>(params)
        .await
        .is_ok_and(|result| {
            (action.profile.is_none() || result.home_selectors)
                && !result.agents.is_empty()
                && result.agents.iter().all(|status| {
                    status.state == IntegrationInstallState::Current
                        && status.recovery == IntegrationRecovery::None
                })
        })
}

/// Installs this release as the service and reads the plan of the installed
/// version, which must match the preview modulo the transitions the install
/// engine performed.
///
/// Returns the installed version directory, its `PATH`, and the plan to apply
/// the rest from.
///
/// # Errors
///
/// Returns [`CliError::SetupInstalledOnly`] when the plan read after the
/// install does not match the preview, and the errors of the install and the
/// plan.
async fn install_service(
    context: &Context,
    from: &Path,
    previewed: &EnsureReport,
    applied: &mut Vec<String>,
    planner: &dyn ReleasePlanner,
) -> Result<(PathBuf, String, EnsureReport), CliError> {
    let installed = service::install_in(context, Some(from.to_path_buf()), None).await?;
    applied.push(format!("service: installed version {}", installed.version));
    applied.extend(engine_changes(installed.runtimes.as_ref()));
    let version_dir = installed
        .daemon_executable
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| CliError::SetupIncomplete {
            detail: "the installed daemon has no version directory".to_owned(),
            applied: Vec::new(),
        })?;
    let installed_search = installed_job_path(context).await?.value;
    let after = planner
        .plan(context, &version_dir, &installed_search)
        .await?;
    if let Err(detail) = check_transitions(previewed, &after, installed.runtimes.as_ref()) {
        return Err(CliError::SetupInstalledOnly {
            version: installed.version,
            prefix: installed.prefix,
            applied: applied.clone(),
            detail,
        });
    }
    Ok((version_dir, installed_search, after))
}

/// Reads the plan of an installed host again after the consent, so the apply
/// acts on the host as it is now, not as it was when the preview was shown.
///
/// The owner may have taken a while to answer, and a profile pinned or a
/// package changed meanwhile would otherwise be overwritten with the preview's
/// choice. The plan must match the preview exactly; nothing is changed
/// otherwise. A change made between this read and the pin it affects is not
/// seen: the pin request cannot name the pin it expects to replace.
///
/// # Errors
///
/// Returns [`CliError::SetupBlocked`] with the first difference, and the
/// errors of the plan.
async fn recheck_present(
    context: &Context,
    plan: &HostPlan,
    planner: &dyn ReleasePlanner,
) -> Result<EnsureReport, CliError> {
    let current = planner.plan(context, &plan.dir, &plan.search_path).await?;
    check_transitions(&plan.previewed, &current, None).map_err(|detail| {
        CliError::SetupBlocked {
            detail: format!(
                "the host changed since the preview ({detail}); re-run `pohunek setup` to see \
                 the new plan"
            ),
        }
    })?;
    Ok(current)
}

/// The changes the install engine's runtime step made, as the install report
/// names them: the packages it installed and the profiles it pinned.
fn engine_changes(runtimes: Option<&EnsureReport>) -> Vec<String> {
    let Some(report) = runtimes else {
        return Vec::new();
    };
    let mut changes = Vec::new();
    for package in &report.packages {
        if wire_name(&package.status) == ENGINE_INSTALLED {
            changes.push(format!("package {}: installed", package.runtime));
        }
    }
    for profile in &report.profiles {
        if wire_name(&profile.status) == ENGINE_BOUND {
            changes.push(format!("profile {}: bound", profile.name));
        }
    }
    if report.catalog == CatalogStatus::Recorded {
        changes.push("catalog trust state: recorded".to_owned());
    }
    changes
}

/// Checks that the plan read after a fresh install matches the preview, and
/// that every difference is a transition the install engine reports.
///
/// The engine may change only what the preview showed. A previewed package may
/// be `present` after the install only when the engine reports it installed,
/// and a previewed package the engine installed must be present. A previewed
/// profile pin may leave the plan only when the engine reports it bound. A
/// recorded catalog is accepted only when the engine reports it recorded.
/// Integrations, gaps and refusals must match.
///
/// # Errors
///
/// Returns the first difference, as the message the stop reports.
fn check_transitions(
    previewed: &EnsureReport,
    after: &EnsureReport,
    engine: Option<&EnsureReport>,
) -> Result<(), String> {
    if !after.refusals.is_empty() {
        return Err(format!(
            "the plan read after the install refuses: {}",
            refusal_text(&after.refusals)
        ));
    }
    if previewed.integrations != after.integrations {
        return Err("the integrations differ from the preview".to_owned());
    }
    if previewed.gaps != after.gaps {
        return Err("the gaps differ from the preview".to_owned());
    }
    check_engine_changes_shown(previewed, engine)?;
    check_package_transitions(previewed, after, engine)?;
    check_profile_transitions(previewed, after, engine)?;
    check_catalog_transition(previewed, after, engine)
}

/// Whether the install engine reports `runtime`'s package installed.
fn engine_installed(engine: Option<&EnsureReport>, runtime: &str) -> bool {
    engine.is_some_and(|report| {
        report.packages.iter().any(|package| {
            package.runtime == runtime && wire_name(&package.status) == ENGINE_INSTALLED
        })
    })
}

/// Whether two profile entries name the same pin: name, base and digest.
fn same_profile(left: &ProfileAction, right: &ProfileAction) -> bool {
    left.name == right.name && left.base == right.base && left.digest == right.digest
}

/// Whether the install engine reports the profile `shown` pinned.
fn engine_bound(engine: Option<&EnsureReport>, shown: &ProfileAction) -> bool {
    engine.is_some_and(|report| {
        report.profiles.iter().any(|profile| {
            same_profile(profile, shown) && wire_name(&profile.status) == ENGINE_BOUND
        })
    })
}

/// Every package and profile the install engine changed was in the preview.
fn check_engine_changes_shown(
    previewed: &EnsureReport,
    engine: Option<&EnsureReport>,
) -> Result<(), String> {
    let Some(report) = engine else {
        return Ok(());
    };
    for package in &report.packages {
        let shown = previewed.packages.iter().any(|shown| {
            shown.runtime == package.runtime && shown.status == PackageStatus::Install
        });
        if wire_name(&package.status) == ENGINE_INSTALLED && !shown {
            return Err(format!(
                "the install engine installed runtime {}, which the preview did not show",
                package.runtime
            ));
        }
    }
    for profile in &report.profiles {
        let shown = previewed
            .profiles
            .iter()
            .any(|shown| same_profile(shown, profile) && shown.status == ProfileStatus::Bind);
        if wire_name(&profile.status) == ENGINE_BOUND && !shown {
            return Err(format!(
                "the install engine pinned profile {}, which the preview did not show",
                profile.name
            ));
        }
    }
    Ok(())
}

/// Each planned package matches its preview, and changes status only as the
/// install engine reports.
fn check_package_transitions(
    previewed: &EnsureReport,
    after: &EnsureReport,
    engine: Option<&EnsureReport>,
) -> Result<(), String> {
    for package in &after.packages {
        let Some(shown) = previewed
            .packages
            .iter()
            .find(|shown| shown.runtime == package.runtime)
        else {
            return Err(format!(
                "runtime {} is planned after the install but was not previewed",
                package.runtime
            ));
        };
        if shown.package != package.package
            || shown.version != package.version
            || shown.digest != package.digest
            || shown.reasons != package.reasons
        {
            return Err(format!(
                "runtime {} changed its package, version, digest or reasons after the install",
                package.runtime
            ));
        }
        let installed = engine_installed(engine, &package.runtime);
        let allowed = match (shown.status, package.status) {
            (PackageStatus::Present, PackageStatus::Present) => true,
            (PackageStatus::Install, PackageStatus::Install) => !installed,
            (PackageStatus::Install, PackageStatus::Present) => installed,
            _ => false,
        };
        if !allowed {
            return Err(format!(
                "runtime {} is {} after the install; the preview showed {}, and the install engine {}",
                package.runtime,
                wire_name(&package.status),
                wire_name(&shown.status),
                if installed {
                    "reports it installed"
                } else {
                    "reports no install"
                }
            ));
        }
    }
    for shown in &previewed.packages {
        if !after
            .packages
            .iter()
            .any(|package| package.runtime == shown.runtime)
        {
            return Err(format!(
                "runtime {} is no longer planned after the install",
                shown.runtime
            ));
        }
    }
    Ok(())
}

/// Each profile pin matches its preview; a pin that leaves the plan must be
/// one the install engine reports bound.
fn check_profile_transitions(
    previewed: &EnsureReport,
    after: &EnsureReport,
    engine: Option<&EnsureReport>,
) -> Result<(), String> {
    for profile in &after.profiles {
        let Some(shown) = previewed
            .profiles
            .iter()
            .find(|shown| shown.name == profile.name)
        else {
            return Err(format!(
                "profile {} is planned after the install but was not previewed",
                profile.name
            ));
        };
        if shown.base != profile.base || shown.digest != profile.digest {
            return Err(format!(
                "profile {} changed its base or digest after the install",
                profile.name
            ));
        }
    }
    for shown in &previewed.profiles {
        let pending = after
            .profiles
            .iter()
            .any(|profile| profile.name == shown.name);
        if !pending && shown.status == ProfileStatus::Bind && !engine_bound(engine, shown) {
            return Err(format!(
                "profile {} left the plan after the install, but the install engine did not pin it",
                shown.name
            ));
        }
    }
    Ok(())
}

/// The catalog trust state changes only as the install engine records it.
fn check_catalog_transition(
    previewed: &EnsureReport,
    after: &EnsureReport,
    engine: Option<&EnsureReport>,
) -> Result<(), String> {
    let engine_recorded = engine.is_some_and(|report| report.catalog == CatalogStatus::Recorded);
    if engine_recorded && after.catalog != CatalogStatus::Current {
        return Err(format!(
            "the install engine recorded the catalog trust state, but the plan after the install shows {}",
            wire_name(&after.catalog)
        ));
    }
    if previewed.catalog == after.catalog {
        return Ok(());
    }
    let recorded = previewed.catalog == CatalogStatus::Record
        && after.catalog == CatalogStatus::Current
        && engine.is_some_and(|report| report.catalog == CatalogStatus::Recorded);
    if recorded {
        Ok(())
    } else {
        Err(format!(
            "the catalog trust state is {} after the install, the preview showed {}",
            wire_name(&after.catalog),
            wire_name(&previewed.catalog)
        ))
    }
}

/// Applies the packages and profile pins the plan still lists, over the
/// daemon's RPCs, recording one line per outcome.
///
/// # Errors
///
/// Returns the errors of the daemon calls, and [`CliError::SetupIncomplete`]
/// for a planned digest or path the request cannot carry.
async fn apply_remaining(
    client: &mut Client,
    dir: &Path,
    remaining: &EnsureReport,
    applied: &mut Vec<String>,
) -> Result<(), CliError> {
    let release_dir = dir.join(RELEASE_RUNTIME_DIR);
    for package in remaining
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
        // An already registered package keeps its recorded flags, so the
        // flags the plan needs are set here, as the release apply sets them.
        if !result.package.enabled {
            client
                .call::<method::PackageSetEnabled>(PackageSetEnabledParams {
                    digest: result.package.digest.clone(),
                    enabled: true,
                })
                .await?;
            applied.push(format!("package {}: enabled", package.runtime));
        }
        if !result.package.selected {
            client
                .call::<method::PackageSelect>(PackageSelectParams {
                    digest: result.package.digest.clone(),
                })
                .await?;
            applied.push(format!("package {}: selected", package.runtime));
        }
    }
    for profile in remaining
        .profiles
        .iter()
        .filter(|profile| profile.status == ProfileStatus::Bind)
    {
        let digest = PackageDigest::parse(&profile.digest).map_err(|_malformed| {
            CliError::SetupIncomplete {
                detail: format!("profile {}: its planned digest is not valid", profile.name),
                applied: Vec::new(),
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
/// Every runtime the preview planned must be present in the plan read after
/// the apply, that plan must carry no refusal, and a runtime it lists but the
/// preview did not is reported. Hook gaps are reported and do not fail.
///
/// # Errors
///
/// Returns [`CliError::SetupIncomplete`] naming every check that is not met.
async fn verdict(
    paths: &Paths,
    previewed: &EnsureReport,
    after: &EnsureReport,
    hooks: &[IntegrationReinstall],
    catalog_recorded_by_install: bool,
) -> Result<Verdict, CliError> {
    let packages_missing: Vec<String> = previewed
        .packages
        .iter()
        .filter(|shown| {
            !after.packages.iter().any(|package| {
                package.runtime == shown.runtime && package.status == PackageStatus::Present
            })
        })
        .map(|shown| shown.runtime.clone())
        .collect();
    let packages_unexpected: Vec<String> = after
        .packages
        .iter()
        .filter(|package| {
            !previewed
                .packages
                .iter()
                .any(|shown| shown.runtime == package.runtime)
        })
        .map(|package| package.runtime.clone())
        .collect();
    let profiles_pending: Vec<String> = previewed
        .profiles
        .iter()
        .filter(|shown| {
            shown.status == ProfileStatus::Bind
                && after
                    .profiles
                    .iter()
                    .any(|profile| profile.name == shown.name)
        })
        .map(|shown| shown.name.clone())
        .collect();
    let hooks_failed = hook_lines(hooks, HOOK_FAILED);
    let hook_gaps = hook_lines(hooks, HOOK_GAP);
    let health =
        health::fetch(LOCAL_HOST, paths)
            .await
            .map_err(|error| CliError::SetupIncomplete {
                detail: format!("the daemon does not answer its health check: {error}"),
                applied: Vec::new(),
            })?;
    let diagnosis = doctor::diagnose(paths).await?;
    let verdict = Verdict {
        packages_missing,
        packages_unexpected,
        profiles_pending,
        refusals: after
            .refusals
            .iter()
            .map(|refusal| format!("{}: {}", refusal.subject, refusal.detail))
            .collect(),
        hooks_failed,
        hook_gaps,
        catalog: wire_name(&after.catalog),
        health: health.status,
        doctor: diagnosis.overall.as_str().to_owned(),
    };
    let problems = verdict_problems(
        &verdict,
        catalog_recorded_by_install && after.catalog == CatalogStatus::Record,
        diagnosis.overall == DoctorStatus::Fail,
    );
    if problems.is_empty() {
        Ok(verdict)
    } else {
        Err(CliError::SetupIncomplete {
            detail: problems.join("; "),
            applied: Vec::new(),
        })
    }
}

/// The reasons a verdict is not met, one per failed check.
fn verdict_problems(
    verdict: &Verdict,
    catalog_unrecorded: bool,
    doctor_failed: bool,
) -> Vec<String> {
    let mut problems = Vec::new();
    if !verdict.packages_missing.is_empty() {
        problems.push(format!(
            "runtimes not present after the apply: {}",
            verdict.packages_missing.join(", ")
        ));
    }
    if !verdict.packages_unexpected.is_empty() {
        problems.push(format!(
            "runtimes planned after the apply but not previewed: {}",
            verdict.packages_unexpected.join(", ")
        ));
    }
    if !verdict.profiles_pending.is_empty() {
        problems.push(format!(
            "profiles still to pin: {}",
            verdict.profiles_pending.join(", ")
        ));
    }
    if !verdict.refusals.is_empty() {
        problems.push(format!(
            "the plan after the apply refuses: {}",
            verdict.refusals.join("; ")
        ));
    }
    if catalog_unrecorded {
        problems.push(
            "the official package install did not record the release catalog's trust state"
                .to_owned(),
        );
    }
    if !verdict.hooks_failed.is_empty() {
        problems.push(format!("hooks failed: {}", verdict.hooks_failed.join("; ")));
    }
    if doctor_failed {
        problems.push("doctor reports a failed check; run `pohunek doctor`".to_owned());
    }
    problems
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

/// Verifies the release in `staged` with the archive's own
/// `packaging/verify-archive`, with the arguments `packaging/install-daemon.sh`
/// gives it, before anything from the release runs.
///
/// A release that carries a `MANIFEST` or a `packaging/` entry is an extracted
/// archive, so the verifier must be there and must accept it: a removed
/// manifest or verifier fails like a modified member. A directory with
/// neither is a development build or a package manager's layout, which ships
/// no manifest; such a directory is indistinguishable from an archive whose
/// every archive file was removed, which the download's attestation, not
/// this check, rules out. The verifier runs only as a regular file of this
/// user or root, reached through directories of this user or root, that no
/// other account can write: the rule it then applies to the whole archive.
/// Every directory above the release obeys it too, except that a root-owned
/// sticky directory such as `/tmp` may be shared.
///
/// # Errors
///
/// Returns [`CliError::SetupBlocked`] with the verifier's reason when the
/// archive is refused or the verifier cannot run.
async fn verify_sealed_archive(staged: &Path, uid: u32) -> Result<(), CliError> {
    use std::os::unix::fs::MetadataExt as _;

    let blocked = |detail: String| CliError::SetupBlocked { detail };
    let manifest = staged.join(ARCHIVE_MANIFEST);
    let verifier = staged.join(ARCHIVE_VERIFIER);
    let packaging = staged.join(ARCHIVE_PACKAGING_DIR);
    let mut sealed = false;
    for path in [&manifest, &packaging] {
        match fs::symlink_metadata(path) {
            Ok(_) => sealed = true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(blocked(format!(
                    "cannot inspect {} of the release archive: {error}",
                    path.display()
                )))
            }
        }
    }
    if !sealed {
        return Ok(());
    }
    // Every directory above the release is closed to other accounts, so none
    // can rename the checked tree away and put its own verifier at this path
    // before `/bin/sh` opens it. In a root-owned sticky directory no other
    // account can move an entry it does not own.
    for ancestor in staged.ancestors().skip(1) {
        let metadata = fs::symlink_metadata(ancestor).map_err(|error| {
            blocked(format!(
                "cannot inspect {}, a directory above the release archive: {error}",
                ancestor.display()
            ))
        })?;
        if !safe_ancestor(
            metadata.uid(),
            metadata.mode(),
            metadata.file_type().is_dir(),
            uid,
            ROOT_UID,
        ) {
            return Err(blocked(format!(
                "{}, a directory above the release archive, is not a directory of this \
                 user or root closed to other accounts; extract the archive below one \
                 that is",
                ancestor.display()
            )));
        }
    }
    // The verifier, and the directories it is reached through, must be
    // closed to other accounts, so none can swap it between this check and
    // its run; the verifier then applies the same rule to the whole archive.
    for (path, directory) in [
        (staged, true),
        (packaging.as_path(), true),
        (verifier.as_path(), false),
    ] {
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            blocked(format!(
                "the release archive has no usable {}: {error}",
                path.display()
            ))
        })?;
        let kind_matches = if directory {
            metadata.file_type().is_dir()
        } else {
            metadata.file_type().is_file()
        };
        if !kind_matches
            || (metadata.uid() != uid && metadata.uid() != ROOT_UID)
            || metadata.mode() & 0o022 != 0
        {
            return Err(blocked(format!(
                "{} is not a {} of this user or root closed to other accounts; \
                 extract the archive again",
                path.display(),
                if directory {
                    "directory"
                } else {
                    "regular file"
                }
            )));
        }
    }
    let output = tokio::process::Command::new("/bin/sh")
        .arg(&verifier)
        .arg(staged)
        .arg(DAEMON_ARCHIVE_COMPONENT)
        .args(DAEMON_ARCHIVE_BINARIES)
        .env_clear()
        .env("PATH", VERIFIER_PATH)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|error| blocked(format!("cannot run {}: {error}", verifier.display())))?;
    if output.status.success() {
        return Ok(());
    }
    let reason = String::from_utf8_lossy(&output.stderr);
    Err(blocked(format!(
        "the release archive {} failed its verification ({ARCHIVE_VERIFIER}): {}",
        staged.display(),
        reason.trim()
    )))
}

/// The version directory of the installed release.
///
/// # Errors
///
/// Returns [`CliError::SetupBlocked`] when the recorded prefix has no version
/// directory for this release.
fn installed_version_dir(status: &service::InstalledFacts) -> Result<PathBuf, CliError> {
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
            applied: Vec::new(),
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
    let preview = &plan.preview;
    let mut out = String::new();
    let _ = writeln!(out, "pohunek setup, version {VERSION}");
    match &preview.service {
        ServiceStep::Install { from } => {
            let _ = writeln!(out, "service: install this release from {from}");
        }
        ServiceStep::Present => {
            let _ = writeln!(out, "service: runs version {VERSION}; kept");
        }
    }
    let _ = writeln!(out, "catalog: {}", wire_name(&preview.catalog));
    if let Some(note) = &preview.catalog_note {
        let _ = writeln!(out, "  note: {note}");
    }
    let _ = writeln!(out, "packages:");
    for package in &preview.packages {
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
    for profile in &preview.profiles {
        let _ = writeln!(
            out,
            "  {} (base {}): {}",
            profile.name,
            profile.base,
            wire_name(&profile.status)
        );
    }
    let _ = writeln!(out, "hooks:");
    for integration in &preview.integrations {
        match &integration.profile {
            Some(profile) => {
                let _ = writeln!(out, "  {} profile {profile}", integration.runtime);
            }
            None => {
                let _ = writeln!(out, "  {}", integration.runtime);
            }
        }
    }
    let _ = writeln!(out, "hermes homes:");
    for home in &preview.hermes_homes {
        let state = match home.status {
            HERMES_UPDATE => "updated (marked by pohunek)".to_owned(),
            HERMES_CURRENT => "kept (current)".to_owned(),
            "gap" => format!("left alone: {}", home.detail.as_deref().unwrap_or_default()),
            _ => format!(
                "cannot be planned: {}",
                home.detail.as_deref().unwrap_or_default()
            ),
        };
        let _ = writeln!(out, "  {}: {state}", home.integration);
    }
    let _ = writeln!(out, "config:");
    for file in &preview.config {
        let state = if file.exists {
            "keep (exists)"
        } else {
            "create"
        };
        let _ = writeln!(out, "  {state}: {}", file.path);
    }
    let _ = writeln!(out, "gaps:");
    for gap in &preview.gaps {
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
    if let Some(note) = &report.preview.catalog_note {
        let _ = writeln!(out, "note: {note}");
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
        "verdict: health {}, doctor {}, catalog {}",
        report.verdict.health, report.verdict.doctor, report.verdict.catalog
    );
    out
}
