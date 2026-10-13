//! Service manager units of workflow plugin jobs and services.
//!
//! Every declared job becomes a periodic unit and every declared service a
//! long-running unit of the user's service manager (systemd user units on
//! Linux, launchd agents on macOS), installed through the platform
//! [`PluginUnits`] backend in the namespace and unit directory of the
//! `pohunek service` installation. Each unit runs
//! `<prefix>/bin/pohunek workflow __exec <id> <digest> job|service <name>`
//! with only the bootstrap path roots in its environment; that trampoline
//! ([`super::supervised`]) decides every run.
//!
//! - **Enable** verifies the plugin, refuses a job outside the owner's
//!   `[jobs]` bounds, marks the plugin enabled, records the names of its units
//!   in the plugin's unit record, then installs and starts every unit. A
//!   failure removes every unit again, forgets their names and marks the
//!   plugin disabled. The registry goes first because a unit starts at once
//!   and its trampoline refuses to run a disabled plugin.
//! - **Disable** marks the plugin disabled first, so the trampoline refuses
//!   from then on, then stops and removes every unit.
//! - **Uninstall** first applies the store's own refusals, then marks the
//!   plugin disabled and removes every unit before the plugin's record, so a
//!   later failure leaves a disabled plugin without units.
//!
//! Disable and uninstall stop the units named by the unit record and, when
//! the installed files verify, the units the manifest declares. The record is
//! what finds the units of a plugin whose files fail verification, so those
//! units stop too. The record is forgotten only after every one of its units
//! was removed or reported absent.
//!
//! All three hold the store's unit lifecycle lock and are safe to repeat. A
//! plugin that declares neither jobs nor services never reaches the service
//! manager, and needs no `pohunek service` installation. Enable requires
//! one, of this CLI's version, because the units run its trampoline. Disable
//! and uninstall without one still look for the plugin's units in the
//! namespace and unit directory this process derives, the ones a
//! `pohunek service install` would record, because nothing yet stops the
//! service from being uninstalled while plugin units exist. When that
//! service manager cannot be reached they warn instead: a unit left behind
//! refuses every run, since the trampoline needs a verified installation,
//! and the record is kept. A warning also names units that cannot be found at
//! all: the installed files fail verification and no unit record exists. An
//! enabling install of a new plugin checks its [`plan`] first and holds the
//! lock from before the plugin is recorded until its units run ([`start`]).

// Rust guideline compliant 2026-10-13

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use package::registry::PackageRecord;
use pohunek_paths::BasePaths;
use pohunek_platform::supervisor::{
    self, Activation, JobDefinition, JobSpec, Namespace, PluginUnitDefinition, PluginUnitKey,
    PluginUnitKind, PluginUnits, RestartPolicy, MAX_JOB_TIMEOUT, MIN_JOB_INTERVAL,
};
use pohunek_service_config::ServiceConfig;
use protocol::{PackageDigest, PackageId};

use super::config::{self, Knobs, MAX_KILL_GRACE_MS, SUPERVISED_MARGIN_MS};
use super::manifest::{Job, Manifest};
use super::runner::kind_name;
use super::store::{Removal, Store, UnitKind, UnitName};
use super::Error;
use crate::paths::WorkflowDirs;
use crate::service::layout::CLI_NAME;
use crate::service::{settings, Context, VERSION};

/// Hidden `pohunek workflow` subcommand every unit runs.
pub(crate) const TRAMPOLINE: &str = "__exec";

/// Timer accuracy of a job unit: the shortest the platform accepts.
///
/// The timer only checks whether a run is due, so coalescing wakeups would
/// only make due runs later; the check period knob sets how often it fires.
const JOB_ACCURACY: Duration = MIN_JOB_INTERVAL;

/// Native restart pacing of a service unit: the shortest the platform
/// accepts. The trampoline applies the owner's backoff before each run, so
/// the native delay is only a backstop against a trampoline that cannot
/// start at all.
const SERVICE_RESTART_DELAY: Duration = MIN_JOB_INTERVAL;

/// Native `SIGTERM`-to-`SIGKILL` grace of a unit: the longest kill grace an
/// owner can set plus [`SUPERVISED_MARGIN_MS`], so a stopped trampoline
/// always has time to stop its child's process group within the owner's
/// grace and record the run.
const UNIT_EXIT_TIMEOUT: Duration = Duration::from_millis(MAX_KILL_GRACE_MS + SUPERVISED_MARGIN_MS);

/// A verified `pohunek service` installation and the CLI its units run.
#[derive(Debug)]
pub(crate) struct Installation {
    /// The recorded installation.
    pub(crate) config: ServiceConfig,
    /// `<prefix>/bin/pohunek`.
    pub(crate) cli: PathBuf,
}

/// Reads and verifies the installation recorded for `paths` and `uid`;
/// `None` when no `service.toml` exists.
///
/// The recorded user and canonical roots must be this process's, as for
/// `pohunek service status`, so units never land in another installation's
/// namespace, and `<prefix>/bin/pohunek` must be a regular file.
///
/// # Errors
///
/// Returns [`Error::ServiceNotInstalled`] when `service.toml` cannot be read,
/// does not describe this process, or the CLI copy is missing.
pub(crate) fn installation_at(paths: &BasePaths, uid: u32) -> Result<Option<Installation>, Error> {
    let not_installed = |reason: String| Error::ServiceNotInstalled { reason };
    let path = pohunek_service_config::file_path(paths);
    match std::fs::symlink_metadata(&path) {
        Ok(_metadata) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(not_installed(format!(
                "{} cannot be inspected: {}",
                path.display(),
                error.kind()
            )));
        }
    }
    let config = ServiceConfig::load_upgrade_source(&path)
        .map_err(|error| not_installed(error.to_string()))?
        .config;
    config
        .verify_installation(uid, &paths.state_dir, &paths.runtime_dir)
        .map_err(|error| not_installed(error.to_string()))?;
    let cli = config.layout().bin_dir().join(CLI_NAME);
    if !std::fs::symlink_metadata(&cli).is_ok_and(|metadata| metadata.is_file()) {
        return Err(not_installed(format!(
            "{} is not an installed pohunek",
            cli.display()
        )));
    }
    Ok(Some(Installation { config, cli }))
}

/// Like [`installation_at`], requiring an installation.
///
/// # Errors
///
/// Returns [`Error::ServiceNotInstalled`] also when no `service.toml` exists.
pub(crate) fn require_installation(context: &Context) -> Result<Installation, Error> {
    installation_at(context.paths(), context.uid())?.ok_or_else(|| Error::ServiceNotInstalled {
        reason: format!("{} does not exist", context.config_path().display()),
    })
}

/// Refuses an installation recorded for another version than this CLI.
///
/// The units run `<prefix>/bin/pohunek workflow __exec`, and only the
/// installed version's trampoline is known to match this CLI. Units installed
/// for another version could never run, so enabling refuses until the
/// installation is this version.
///
/// # Errors
///
/// Returns [`Error::ServiceNotInstalled`] naming both versions and the
/// recovery.
fn require_same_version(installation: &Installation) -> Result<(), Error> {
    let recorded = installation.config.active_version();
    if recorded == VERSION {
        return Ok(());
    }
    Err(Error::ServiceNotInstalled {
        reason: format!(
            "the recorded pohunek service installation is version {recorded}, but this pohunek \
             is version {VERSION}; install the service from this version with \
             `pohunek service install`, or upgrade the installation to it"
        ),
    })
}

/// The host facts the unit operations need, resolved only when a plugin
/// declares units, so actions-only plugins work without them.
#[derive(Debug)]
pub(crate) struct Host {
    context: Option<Context>,
}

impl Host {
    /// Resolves the context from this process's environment on first use.
    pub(crate) fn resolve_lazily() -> Self {
        Self { context: None }
    }

    /// Uses `context`.
    pub(crate) fn given(context: Context) -> Self {
        Self {
            context: Some(context),
        }
    }

    fn context(&mut self) -> Result<&Context, Error> {
        if self.context.is_none() {
            let context = Context::resolve().map_err(|error| Error::ServiceNotInstalled {
                reason: error.to_string(),
            })?;
            self.context = Some(context);
        }
        Ok(self
            .context
            .as_ref()
            .expect("the context was resolved above"))
    }
}

/// The names of every job and service `manifest` declares.
fn manifest_units(manifest: &Manifest) -> Vec<UnitName> {
    let jobs = manifest.jobs.keys().map(|name| UnitName {
        kind: UnitKind::Job,
        name: name.clone(),
    });
    let services = manifest.services.keys().map(|name| UnitName {
        kind: UnitKind::Service,
        name: name.clone(),
    });
    jobs.chain(services).collect()
}

/// Refuses a job of `manifest` outside the owner's `[jobs]` bounds.
///
/// # Errors
///
/// Returns [`Error::UnitRefused`] naming the job and the knob it breaks.
pub(crate) fn check_bounds(manifest: &Manifest, knobs: &Knobs) -> Result<(), Error> {
    manifest
        .jobs
        .iter()
        .try_for_each(|(name, job)| check_job(&manifest.identity.id, name, job, knobs))
}

/// Refuses job `name` of plugin `id` when its `every_s` is below
/// `jobs.min_interval_s` or its `timeout_ms` above `jobs.timeout_max_ms`;
/// checked when units are installed and again before each run.
///
/// # Errors
///
/// Returns [`Error::UnitRefused`] naming the job and the knob it breaks.
pub(crate) fn check_job(id: &PackageId, name: &str, job: &Job, knobs: &Knobs) -> Result<(), Error> {
    let jobs = &knobs.jobs;
    let refused = |reason: String| Error::UnitRefused {
        id: id.to_string(),
        unit: format!("job {name}"),
        reason,
    };
    let every = job.every_s.get();
    if every < jobs.min_interval_s {
        return Err(refused(format!(
            "declares every_s {every}, below jobs.min_interval_s {}",
            jobs.min_interval_s
        )));
    }
    match job.timeout_ms.map(u64::from) {
        Some(timeout) if timeout > jobs.timeout_max_ms => Err(refused(format!(
            "declares timeout_ms {timeout}, above jobs.timeout_max_ms {}",
            jobs.timeout_max_ms
        ))),
        _ => Ok(()),
    }
}

/// A unit of a plan: its record name, platform key and definition.
#[derive(Debug)]
struct PlannedUnit {
    name: UnitName,
    key: PluginUnitKey,
    definition: PluginUnitDefinition,
}

/// Builds every unit definition of `record`'s `manifest`, refusing a job
/// outside the owner's `[jobs]` bounds before anything changes.
fn definitions(
    digest: &PackageDigest,
    manifest: &Manifest,
    knobs: &Knobs,
    installation: &Installation,
    environment: &BTreeMap<String, String>,
    home: &Path,
) -> Result<Vec<PlannedUnit>, Error> {
    let id = &manifest.identity.id;
    check_bounds(manifest, knobs)?;
    let mut units = Vec::new();
    for (name, job) in &manifest.jobs {
        let activation = Activation::Interval {
            every: Duration::from_secs(job.every_s.get().min(knobs.jobs.check_period_s)),
            accuracy: JOB_ACCURACY,
        };
        units.push((PluginUnitKind::Job, name, activation));
    }
    for name in manifest.services.keys() {
        units.push((PluginUnitKind::Service, name, Activation::Service));
    }
    units
        .into_iter()
        .map(|(kind, name, activation)| {
            let key = PluginUnitKey::new(id.as_str(), kind, name.as_str())
                .expect("manifest names fit the unit name grammar");
            let spec = JobSpec {
                executable: installation.cli.clone(),
                arguments: [
                    "workflow",
                    TRAMPOLINE,
                    id.as_str(),
                    digest.as_str(),
                    kind_name(kind),
                    name.as_str(),
                ]
                .map(str::to_owned)
                .to_vec(),
                environment: environment.clone(),
                working_directory: home.to_path_buf(),
                logs: None,
                // The trampoline bounds a job run below this; see
                // `config::MAX_JOB_TIMEOUT_MS`.
                start_timeout: MAX_JOB_TIMEOUT,
                exit_timeout: UNIT_EXIT_TIMEOUT,
                restart: match kind {
                    PluginUnitKind::Job => RestartPolicy::Never,
                    PluginUnitKind::Service => RestartPolicy::Always {
                        delay: SERVICE_RESTART_DELAY,
                    },
                },
                open_files: installation.config.open_files(),
            };
            let definition = JobDefinition::new(spec)
                .and_then(|job| PluginUnitDefinition::new(job, activation))
                .map_err(|error| Error::Units {
                    id: id.to_string(),
                    detail: format!("the {} {name} unit is invalid: {error}", kind_name(kind)),
                })?;
            Ok(PlannedUnit {
                name: UnitName {
                    kind: UnitKind::of(kind),
                    name: name.clone(),
                },
                key,
                definition,
            })
        })
        .collect()
}

/// Connects the service manager backend of `installation`.
fn connect<'a>(
    context: &'a Context,
    installation: &Installation,
    id: &'a PackageId,
) -> impl std::future::Future<Output = Result<Box<dyn PluginUnits>, Error>> + 'a {
    connect_namespace(
        context,
        installation.config.namespace(),
        installation.config.deadlines().launchctl_command,
        id,
    )
}

/// Connects the service manager backend of `namespace` in `context`'s unit
/// directory; every `launchctl` command ends after `launchctl_deadline`
/// (macOS only).
#[cfg(target_os = "linux")]
async fn connect_namespace(
    context: &Context,
    namespace: Namespace,
    _launchctl_deadline: Duration,
    id: &PackageId,
) -> Result<Box<dyn PluginUnits>, Error> {
    use pohunek_platform::supervisor::systemd::SystemdPluginUnits;

    SystemdPluginUnits::connect(
        namespace,
        context.supervisor_dir().to_path_buf(),
        settings::SUPERVISOR_CALL_TIMEOUT,
    )
    .await
    .map(|units| Box::new(units) as Box<dyn PluginUnits>)
    .map_err(|error| units_failed(id, format!("the service manager is unreachable: {error}")))
}

/// Connects the service manager backend of `namespace` in `context`'s unit
/// directory; every `launchctl` command ends after `launchctl_deadline`.
#[cfg(target_os = "macos")]
fn connect_namespace(
    context: &Context,
    namespace: Namespace,
    launchctl_deadline: Duration,
    id: &PackageId,
) -> std::future::Ready<Result<Box<dyn PluginUnits>, Error>> {
    use pohunek_platform::supervisor::launchd::LaunchdPluginUnits;

    // launchd needs no connection, so the backend is ready at once; the
    // future keeps one call shape with the systemd arm.
    std::future::ready(
        LaunchdPluginUnits::new(
            namespace,
            context.uid(),
            context.supervisor_dir().to_path_buf(),
            launchctl_deadline,
        )
        .map(|units| Box::new(units) as Box<dyn PluginUnits>)
        .map_err(|error| units_failed(id, format!("the service manager is unusable: {error}"))),
    )
}

fn units_failed(id: &PackageId, detail: String) -> Error {
    Error::Units {
        id: id.to_string(),
        detail,
    }
}

/// The units of one plugin archive, checked and ready to install.
#[derive(Debug)]
pub(crate) struct Plan {
    installation: Installation,
    units: Vec<PlannedUnit>,
}

impl Plan {
    /// The names of the units, as the unit record lists them.
    fn names(&self) -> Vec<UnitName> {
        self.units.iter().map(|unit| unit.name.clone()).collect()
    }
}

/// Checks everything installing the units of `manifest` (archive `digest`)
/// needs, without changing anything or contacting the service manager: the
/// service installation and its environment, and the owner's `[jobs]`
/// bounds.
///
/// # Errors
///
/// Returns [`Error::ServiceNotInstalled`], [`Error::UnitRefused`] or
/// [`Error::Units`] for a definition the platform refuses.
pub(crate) fn plan(
    context: &Context,
    digest: &PackageDigest,
    manifest: &Manifest,
    knobs: &Knobs,
) -> Result<Plan, Error> {
    let installation = require_installation(context)?;
    require_same_version(&installation)?;
    let environment =
        context
            .service_environment()
            .map_err(|error| Error::ServiceNotInstalled {
                reason: format!("the service environment is unusable: {error}"),
            })?;
    let home = environment
        .get(pohunek_paths::HOME)
        .map(PathBuf::from)
        .expect("the service environment requires HOME");
    let units = definitions(digest, manifest, knobs, &installation, &environment, &home)?;
    Ok(Plan {
        installation,
        units,
    })
}

/// A failed [`start`] and whether it was undone.
#[derive(Debug)]
pub(crate) struct StartFailed {
    /// Why it failed.
    pub(crate) error: Error,
    /// Whether the plugin is disabled with no units, as before the start.
    /// `false` only when undoing a failed unit install failed too, so the
    /// plugin may remain enabled or keep units.
    pub(crate) undone: bool,
}

/// Marks plugin `id` enabled, records the names of the units of `plan`, and
/// installs and starts those units, under the unit lifecycle lock the caller
/// holds. A unit that cannot be installed removes every unit again, forgets
/// their names and disables the plugin again.
///
/// # Errors
///
/// Returns [`Error::Units`] when the service manager cannot be reached
/// (nothing changed) or a unit cannot be installed (rolled back). When the
/// plugin cannot be marked enabled, [`Error::Busy`], [`Error::RootInvalid`]
/// and [`Error::PluginUnknown`] change nothing; any other
/// [`Store::set_enabled`] error may follow a committed flag, so the plugin is
/// disabled again and `undone` reports whether that succeeded. A unit record
/// that cannot be written is refused before any unit is installed.
pub(crate) async fn start(
    store: &Store,
    context: &Context,
    id: &PackageId,
    plan: &Plan,
) -> Result<PackageRecord, StartFailed> {
    let unchanged = |error| StartFailed {
        error,
        undone: true,
    };
    let backend = connect(context, &plan.installation, id)
        .await
        .map_err(unchanged)?;
    // Busy, an unknown plugin and an unverifiable root refuse before the flag
    // is written. Any other error may follow a committed write, so the flag is
    // cleared again; `set_enabled(false)` is Ok for a plugin already disabled,
    // which covers the case where nothing was committed.
    let enabled = match store.set_enabled(id, true) {
        Ok(record) => record,
        Err(error @ (Error::Busy | Error::PluginUnknown { .. } | Error::RootInvalid { .. })) => {
            return Err(unchanged(error));
        }
        Err(error) => {
            let undone = store.set_enabled(id, false).is_ok();
            return Err(StartFailed { error, undone });
        }
    };
    let names = plan.names();
    // The names are recorded before the first install, so no installed unit is
    // missing from the record, even when the process stops in between.
    if let Err(error) = store.record_units(id, &names) {
        let undone = store.set_enabled(id, false).is_ok();
        return Err(StartFailed { error, undone });
    }
    for unit in &plan.units {
        if let Err(error) = backend.install(&unit.key, &unit.definition).await {
            let mut detail = format!("installing the {} unit failed: {error}", unit.key);
            let mut undone = true;
            for key in plan.units.iter().map(|unit| &unit.key) {
                if let Err(error) = backend.remove(key).await {
                    undone = false;
                    let _ = write!(detail, "; removing the {key} unit failed too: {error}");
                }
            }
            // The record keeps the names when a unit may still exist, so a
            // later disable or uninstall still stops it.
            if undone {
                if let Err(error) = store.forget_units(id, &names) {
                    undone = false;
                    let _ = write!(detail, "; the unit record could not be updated: {error}");
                }
            }
            match store.set_enabled(id, false) {
                Ok(_record) => detail.push_str("; the plugin was disabled again"),
                Err(error) => {
                    undone = false;
                    let _ = write!(
                        detail,
                        "; disabling the plugin again failed, so it may remain enabled: {error}"
                    );
                }
            }
            return Err(StartFailed {
                error: units_failed(id, detail),
                undone,
            });
        }
    }
    Ok(enabled)
}

/// Enables plugin `id` and installs and starts its units.
///
/// # Errors
///
/// Returns [`Error::PluginUnknown`], [`Error::Busy`], [`Error::RootInvalid`],
/// [`Error::ConfigInvalid`], [`Error::ServiceNotInstalled`],
/// [`Error::UnitRefused`] (nothing changed), [`Error::Units`] after every
/// unit was removed and the plugin disabled again, or [`Error::Store`].
pub(crate) async fn enable(
    store: &Store,
    host: &mut Host,
    id: &PackageId,
) -> Result<PackageRecord, Error> {
    let _lock = store
        .lock_units()?
        .ok_or_else(|| Error::PluginUnknown { id: id.to_string() })?;
    let (record, _root, manifest) = store.inspect(id)?;
    if !manifest.has_units() {
        return store.set_enabled(id, true);
    }
    let knobs = config::load(store.knobs_path())?;
    let context = host.context()?;
    let plan = plan(context, record.digest(), &manifest, &knobs)?;
    start(store, context, id, &plan)
        .await
        .map_err(|failed| failed.error)
}

/// Disables plugin `id`, then stops and removes its units.
///
/// Returns the updated record and warnings about units that cannot be found:
/// the installed files fail verification and no unit record names any unit.
///
/// # Errors
///
/// Returns [`Error::PluginUnknown`], [`Error::Busy`], [`Error::Store`], or,
/// after the plugin was disabled, [`Error::ServiceNotInstalled`] for an
/// unusable installation and [`Error::Units`] for units that could not be
/// removed.
pub(crate) async fn disable(
    store: &Store,
    host: &mut Host,
    id: &PackageId,
) -> Result<(PackageRecord, Vec<String>), Error> {
    let _lock = store
        .lock_units()?
        .ok_or_else(|| Error::PluginUnknown { id: id.to_string() })?;
    let record = store.set_enabled(id, false)?;
    let mut warnings = Vec::new();
    remove_units(store, host, id, &mut warnings).await?;
    Ok((record, warnings))
}

/// Prepares the uninstall of plugin `id` under the unit lifecycle lock the
/// caller holds until the record is removed: applies the store's refusals
/// that depend on the installed files, marks the plugin disabled, and
/// removes its units, so a refusal changes nothing and a later failure
/// leaves a disabled plugin without units.
///
/// # Errors
///
/// Returns the refusals of [`Store::check_removable`], [`Error::Busy`],
/// [`Error::Store`], and the unit errors of [`disable`].
pub(crate) async fn retire(
    store: &Store,
    host: &mut Host,
    id: &PackageId,
    remove_modified: bool,
    warnings: &mut Vec<String>,
) -> Result<(), Error> {
    store.check_removable(id, remove_modified)?;
    store.set_enabled(id, false)?;
    remove_units(store, host, id, warnings).await
}

/// Uninstalls plugin `id` under the unit lifecycle lock: retires its units
/// when the plugin is recorded, or removes the units its unit record still
/// names when it is not, then removes the record and, with `purge`, its
/// directories. The lock is taken in creating mode with `purge`, since
/// purging removes directories outside the store root.
///
/// # Errors
///
/// Returns [`Error::PluginUnknown`] when there is no store and no `purge`,
/// [`Error::Busy`], and the errors of [`retire`] and [`Store::uninstall`].
pub(crate) async fn uninstall(
    store: &Store,
    host: &mut Host,
    id: &PackageId,
    purge: bool,
    remove_modified: bool,
) -> Result<Removal, Error> {
    let lock = if purge {
        Some(store.lock_units_creating()?)
    } else {
        store.lock_units()?
    };
    let Some(lock) = lock else {
        return Err(Error::PluginUnknown { id: id.to_string() });
    };
    let mut warnings = Vec::new();
    if store.installed(id)?.is_some() {
        retire(store, host, id, remove_modified, &mut warnings).await?;
    } else if !store.recorded_units(id)?.is_empty() {
        // Units recorded for a plugin that is no longer recorded are still
        // stopped and removed, so the unit record can always be emptied.
        remove_units(store, host, id, &mut warnings).await?;
    }
    // The unit record goes with the plugin record only once it is empty, so
    // a unit that is still recorded keeps the plugin recorded too.
    if !store.recorded_units(id)?.is_empty() {
        return Err(units_failed(
            id,
            "its units are still recorded, so the plugin was kept".to_owned(),
        ));
    }
    let mut removal = store.uninstall(id, purge, remove_modified)?;
    drop(lock);
    warnings.append(&mut removal.warnings);
    removal.warnings = warnings;
    Ok(removal)
}

/// Removes the units of plugin `id`, under the unit lifecycle lock the
/// caller holds.
///
/// The units are the ones the unit record names and, when the installed files
/// verify, the ones the manifest declares. They are looked up in the recorded
/// installation's namespace, or, without a `service.toml`, in the namespace
/// this process derives (see the module documentation).
///
/// # Errors
///
/// See [`disable`].
async fn remove_units(
    store: &Store,
    host: &mut Host,
    id: &PackageId,
    warnings: &mut Vec<String>,
) -> Result<(), Error> {
    let recorded = store.recorded_units(id)?;
    // `Ok(None)`: the plugin is not recorded, so only the unit record names
    // its units.
    let manifest = match store.inspect(id) {
        Ok((_record, _root, manifest)) => Ok(Some(manifest)),
        Err(Error::PluginUnknown { .. }) => Ok(None),
        Err(Error::RootInvalid { cause, .. }) => Err(cause.to_string()),
        Err(Error::DescriptorInvalid(invalid)) => Err(invalid.to_string()),
        Err(error) => return Err(error),
    };
    let mut names = recorded;
    if let Ok(Some(manifest)) = &manifest {
        names.extend(manifest_units(manifest));
    }
    names.sort();
    names.dedup();
    if names.is_empty() {
        if let Err(cause) = &manifest {
            warnings.push(unlisted_warning(cause));
        }
        return Ok(());
    }
    let installation = host
        .context()
        .and_then(|context| installation_at(context.paths(), context.uid()))?;
    if let Some(installation) = installation {
        let context = host.context()?;
        let backend = connect(context, &installation, id).await?;
        return remove_names(store, backend.as_ref(), id, &names).await;
    }
    let context = host.context()?;
    let namespace = context
        .namespace()
        .map_err(|error| Error::ServiceNotInstalled {
            reason: error.to_string(),
        })?;
    let unreachable = |cause: &dyn std::fmt::Display| {
        format!(
            "no pohunek service installation is recorded and the service manager could not \
             be reached ({cause}), so leftover units of the plugin were not looked up; a unit \
             left behind refuses every run"
        )
    };
    let backend = match connect_namespace(context, namespace, settings::LAUNCHCTL_COMMAND, id).await
    {
        Ok(backend) => backend,
        Err(error) => {
            warnings.push(unreachable(&error));
            return Ok(());
        }
    };
    let mut failures = Vec::new();
    for unit in &names {
        let key = unit.key(id)?;
        match backend.remove(&key).await {
            Ok(()) => {}
            // A manager without this user's domain or bus, such as launchd
            // over SSH, holds no unit of it.
            Err(
                error @ (supervisor::Error::DomainUnavailable { .. }
                | supervisor::Error::Unavailable { .. }
                | supervisor::Error::Timeout { .. }),
            ) => {
                warnings.push(unreachable(&error));
                return Ok(());
            }
            Err(error) => failures.push(format!("removing the {key} unit failed: {error}")),
        }
    }
    finish_removal(store, id, &names, &failures)
}

/// Removes every unit of `names` through `backend`, then forgets them.
async fn remove_names(
    store: &Store,
    backend: &dyn PluginUnits,
    id: &PackageId,
    names: &[UnitName],
) -> Result<(), Error> {
    let mut failures = Vec::new();
    for unit in names {
        let key = unit.key(id)?;
        if let Err(error) = backend.remove(&key).await {
            failures.push(format!("removing the {key} unit failed: {error}"));
        }
    }
    finish_removal(store, id, names, &failures)
}

/// Forgets `names` in the unit record once every one was removed. A failed
/// removal is reported and keeps the record, so a later removal finds the
/// units again.
fn finish_removal(
    store: &Store,
    id: &PackageId,
    names: &[UnitName],
    failures: &[String],
) -> Result<(), Error> {
    if !failures.is_empty() {
        return Err(units_failed(id, failures.join("; ")));
    }
    store.forget_units(id, names)
}

/// The warning for a plugin whose units cannot be found: its installed files
/// fail verification and no unit record names any unit.
fn unlisted_warning(cause: &str) -> String {
    format!(
        "the plugin's jobs and services could not be listed because its installed files \
         fail verification ({cause}) and no unit record names them; a unit left behind \
         refuses every run"
    )
}

/// A failed workflow unit lifecycle step: the stable error code and the
/// message `pohunek workflow` reports for it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct LifecycleError {
    /// The stable error code, as in `pohunek workflow --json` errors.
    pub code: &'static str,
    /// The human-readable message.
    pub message: String,
}

impl From<Error> for LifecycleError {
    fn from(error: Error) -> Self {
        Self {
            code: error.code(),
            message: error.to_string(),
        }
    }
}

/// The workflow plugin store of `context`'s paths.
fn store_of(context: &Context) -> Result<Store, Error> {
    let paths = context.paths();
    WorkflowDirs::new(&paths.state_dir, &paths.config_dir).map(Store::new)
}

/// Enables workflow plugin `plugin` of `context`'s installation and installs
/// its units in `context`'s unit directory, exactly as
/// `pohunek workflow enable` does for the context it resolves.
///
/// # Errors
///
/// Returns the code and message `pohunek workflow enable` reports.
pub async fn enable_plugin(context: &Context, plugin: &str) -> Result<(), LifecycleError> {
    let id = parse_id(plugin)?;
    let store = store_of(context)?;
    enable(&store, &mut Host::given(context.clone()), &id).await?;
    Ok(())
}

/// Disables workflow plugin `plugin` of `context`'s installation and removes
/// its units from `context`'s unit directory, exactly as
/// `pohunek workflow disable` does for the context it resolves. Returns the
/// warnings the command prints.
///
/// # Errors
///
/// Returns the code and message `pohunek workflow disable` reports.
pub async fn disable_plugin(
    context: &Context,
    plugin: &str,
) -> Result<Vec<String>, LifecycleError> {
    let id = parse_id(plugin)?;
    let store = store_of(context)?;
    let (_record, warnings) = disable(&store, &mut Host::given(context.clone()), &id).await?;
    Ok(warnings)
}

/// Uninstalls workflow plugin `plugin` of `context`'s installation, exactly as
/// `pohunek workflow uninstall` does for the context it resolves. Returns the
/// warnings the command prints.
///
/// # Errors
///
/// Returns the code and message `pohunek workflow uninstall` reports.
pub async fn uninstall_plugin(
    context: &Context,
    plugin: &str,
    purge: bool,
    remove_modified: bool,
) -> Result<Vec<String>, LifecycleError> {
    let id = parse_id(plugin)?;
    let store = store_of(context)?;
    let removal = uninstall(
        &store,
        &mut Host::given(context.clone()),
        &id,
        purge,
        remove_modified,
    )
    .await?;
    Ok(removal.warnings)
}

fn parse_id(plugin: &str) -> Result<PackageId, LifecycleError> {
    PackageId::parse(plugin).map_err(|error| LifecycleError {
        code: "cli_usage",
        message: error.to_string(),
    })
}
