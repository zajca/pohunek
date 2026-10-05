//! Integration handlers: the compiled code that owns one runtime's active
//! upstream asset set.
//!
//! A runtime definition names its handler by id (`integration.handler`) and a
//! compiled hook schema (`integration.hook_schema`). The handler registry is
//! the closed set of ids this build provides; a descriptor never carries
//! installer logic, filesystem targets, or argv. The handler decides the
//! accepted target paths, modes, conflict detection, and rollback.
//!
//! An update is a transaction in four steps: [`DaemonHandler::stage`] computes
//! the new asset set against the active one without changing it,
//! [`DaemonHandler::verify_compatibility`] proves the new set against the
//! runtime's hook schema and with the schema of every package version that a
//! session or profile pin still references ([`RetainedSchemas`]),
//! [`StagedUpdate::activate`] switches to it atomically, and any failure while
//! activating restores the exact prior tree before the error is returned. The
//! active set stays untouched until activation succeeds, and a staged set that
//! is dropped changes nothing.
//!
//! A handler never decides where its asset set lives. The config home comes
//! from the runtime descriptor and the environment a launch would give the agent
//! ([`ConfigHomes`]), so each home a runtime can launch with is its own
//! transaction.

// Rust guideline compliant 2026-10-05

use std::collections::BTreeSet;
use std::fmt::Debug;
use std::path::Path;

use pohunek_worker_protocol::{HookAction, HookSchema};
use protocol::{
    BindingProvenance, ErrorClass, IntegrationAgentStatus, IntegrationHomeFailure,
    IntegrationInstallReport, IntegrationInstallResult, IntegrationStatusParams,
    IntegrationStatusResult, IntegrationUninstallReport, PackageId, ProtocolError, RuntimeId,
    RuntimeRef,
};

use super::commit::StepGate;
use super::homes::{ConfigHomes, HomeSelection, Target};
use super::{config_dir_is_symlink, config_path_kind, report, ConfigPath, InstallPaths};
use crate::agent::host::{ConfigHome, RuntimeHost};

/// Error code for an update whose asset set is not compatible with the active
/// set or with the runtime's hook schema.
pub(super) const UPDATE_INCOMPATIBLE_CODE: &str = "integration_update_incompatible";

/// What a staged update installs and what it replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetManifest {
    /// Version marker of the asset set the update installs.
    pub version: u32,
    /// Version marker of the active asset set, when its state hook is readable
    /// and carries a valid marker. An active set of any version is replaceable,
    /// so rolling a release back reinstalls its older set.
    pub active_version: Option<u32>,
    /// Hook operations the new asset set reports to the daemon, derived from
    /// the registration the staged set would activate.
    pub actions: Vec<HookAction>,
}

/// A computed, verified-or-not asset set that is not active yet.
pub trait StagedUpdate: Debug {
    /// The asset set this update installs and the one it replaces.
    fn manifest(&self) -> &AssetManifest;

    /// Switches the active set to the staged one as one transaction, running
    /// `gate` before each committed file.
    ///
    /// A failure restores the exact prior tree; when the restore itself cannot
    /// finish the error is `integration_recovery_required` and names what
    /// stays in quarantine.
    ///
    /// # Errors
    ///
    /// Returns the failing step's error after a complete rollback, or the
    /// recovery error of an incomplete one.
    fn activate(self: Box<Self>, gate: StepGate<'_>) -> Result<InstallPaths, ProtocolError>;
}

/// A handler whose lifecycle the daemon runs.
pub trait DaemonHandler: Send + Sync + Debug {
    /// Registry id a runtime descriptor names.
    fn id(&self) -> &'static str;

    /// Hook operations the handler's asset set reports.
    fn reported_actions(&self) -> &'static [HookAction];

    /// Child directories of the config directory that can hold installer
    /// quarantine entries.
    fn quarantine_subdirs(&self) -> &'static [&'static str];

    /// Read-only status of the active set in the config directory `home`,
    /// worded for the provider the handler manages and reported for that
    /// provider's own runtime.
    ///
    /// A `home` that could not be resolved degrades into a warning of the
    /// report. The warnings are the diagnostic text diagnosis classifies, so
    /// they carry no id of the runtime a request addressed.
    fn inspect_provider(&self, home: Result<&Path, &ProtocolError>) -> IntegrationAgentStatus;

    /// Read-only status of the active set in `home`, reported for `runtime`,
    /// with the recovery commands in its warnings naming `runtime`.
    fn inspect(
        &self,
        runtime: &RuntimeRef,
        home: Result<&Path, &ProtocolError>,
    ) -> IntegrationAgentStatus {
        let mut status = self.inspect_provider(home);
        retarget_status(&mut status, runtime);
        status
    }

    /// Computes the asset set for the config directory `dir` and holds the
    /// installer lock; nothing in `dir` changes.
    ///
    /// # Errors
    ///
    /// Returns `agent_config_dir_missing` for an absent directory, a path
    /// error for an unsafe one, `integration_install_in_progress` when another
    /// installer holds the lock, or the settings error of a malformed
    /// registration file.
    fn stage(&self, dir: &Path) -> Result<Box<dyn StagedUpdate>, ProtocolError>;

    /// Proves that `staged` is compatible with the hook schema `schema` the
    /// runtime definition names.
    ///
    /// The schema must be driven by this handler and admit every operation the
    /// staged registration makes the scripts report, as derived from the staged
    /// content and not from the handler's declaration.
    ///
    /// # Errors
    ///
    /// Returns `integration_update_incompatible` naming the broken condition.
    fn verify_compatibility(
        &self,
        staged: &dyn StagedUpdate,
        schema: &HookSchema,
    ) -> Result<(), ProtocolError> {
        admits(self.id(), staged.manifest(), schema).map_err(|detail| update_incompatible(&detail))
    }

    /// Removes exactly what the handler installed, reported for `runtime`.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for an unsafe config path, the
    /// transaction errors of the commit machinery, or an I/O error.
    fn uninstall(
        &self,
        runtime: &RuntimeRef,
        dir: &Path,
    ) -> Result<IntegrationUninstallReport, ProtocolError>;
}

/// Why `schema` cannot carry the asset set `manifest` of handler `handler`:
/// the handler must drive the schema and the schema must admit every operation
/// the set reports.
fn admits(handler: &str, manifest: &AssetManifest, schema: &HookSchema) -> Result<(), String> {
    if !schema.supports_handler(handler) {
        return Err(format!(
            "hook schema {} is not driven by integration handler {handler}",
            schema.id
        ));
    }
    if let Some(action) = manifest
        .actions
        .iter()
        .find(|action| !schema.allows(**action))
    {
        return Err(format!(
            "the new asset set reports {action}, which hook schema {} does not admit",
            schema.id
        ));
    }
    Ok(())
}

/// The hook schemas of the installed package versions that live, lost or
/// resumable sessions and host profile pins still reference.
///
/// A session keeps reporting through the schema it was launched under, so an
/// asset set that replaces the active one must be admitted by each of them, not
/// only by the schema of the version fresh launches use. Sessions of built-in
/// runtimes are not package pins; their schema is the one this build compiles.
#[derive(Debug, Clone, Default)]
pub struct RetainedSchemas {
    schemas: Vec<(String, &'static HookSchema)>,
    unresolvable: BTreeSet<PackageId>,
}

impl RetainedSchemas {
    /// Records that a retained package version names `handler` with `schema`.
    pub fn push(&mut self, handler: String, schema: &'static HookSchema) {
        self.schemas.push((handler, schema));
    }

    /// Records a retained version of `package` whose integration cannot be
    /// determined. The version may drive any handler, so no handler's asset
    /// set is updated while it is retained.
    pub fn push_unresolvable(&mut self, package: PackageId) {
        self.unresolvable.insert(package);
    }

    /// The schemas of the retained versions that name `handler`.
    pub fn for_handler<'a>(
        &'a self,
        handler: &'a str,
    ) -> impl Iterator<Item = &'static HookSchema> + 'a {
        self.schemas
            .iter()
            .filter(move |(named, _schema)| named == handler)
            .map(|(_named, schema)| *schema)
    }

    /// Whether a retained version exists whose handler and schema are unknown.
    fn has_unresolvable(&self) -> bool {
        !self.unresolvable.is_empty()
    }
}

/// A handler whose lifecycle the `pohunek` CLI runs on the operator's host.
///
/// The daemon registers its id and the operations its asset set reports so a
/// descriptor can name it, but runs none of its operations.
#[derive(Debug)]
pub struct CliRunHandler {
    /// Registry id a runtime descriptor names.
    pub id: &'static str,
    /// Hook operations the handler's asset set reports.
    pub actions: &'static [HookAction],
}

/// One entry of the closed handler registry.
#[derive(Debug, Clone, Copy)]
pub enum Handler {
    /// The daemon runs the handler's lifecycle.
    Daemon(&'static dyn DaemonHandler),
    /// The CLI runs the handler's lifecycle.
    CliRun(&'static CliRunHandler),
}

impl Handler {
    /// Registry id a runtime descriptor names.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Daemon(handler) => handler.id(),
            Self::CliRun(handler) => handler.id,
        }
    }

    /// Hook operations the handler's asset set reports.
    #[must_use]
    pub fn reported_actions(self) -> &'static [HookAction] {
        match self {
            Self::Daemon(handler) => handler.reported_actions(),
            Self::CliRun(handler) => handler.actions,
        }
    }
}

/// Resolves a handler id against the closed registry.
#[must_use]
pub(super) fn handler(id: &str) -> Option<Handler> {
    handlers()
        .iter()
        .copied()
        .find(|handler| handler.id() == id)
}

/// Every registered handler, in a stable order.
#[must_use]
pub(super) fn handlers() -> &'static [Handler] {
    super::provider_handlers::REGISTRY.as_slice()
}

/// Command-line flag the recovery commands in diagnostic text select a runtime
/// with.
const AGENT_FLAG: &str = "--agent";

/// Command-line flag the recovery commands in diagnostic text select a host
/// profile's config home with.
const PROFILE_FLAG: &str = "--profile";

/// Rewrites the recovery commands in `text` that select the runtime `from` to
/// select `to`.
///
/// Only the flag and its value are rewritten, so diagnostic wording is never
/// changed by a runtime id.
pub(super) fn retarget_text(text: &str, from: &str, to: &str) -> String {
    text.replace(
        &format!("{AGENT_FLAG} {from}"),
        &format!("{AGENT_FLAG} {to}"),
    )
}

/// Reports `status` for `runtime` instead of the provider's own runtime.
pub(super) fn retarget_status(status: &mut IntegrationAgentStatus, runtime: &RuntimeRef) {
    let from = status.agent.as_wire().to_owned();
    if from != runtime.as_wire() {
        for warning in &mut status.warnings {
            *warning = retarget_text(warning, &from, runtime.as_wire());
        }
    }
    status.agent = runtime.clone();
}

/// `integration_update_incompatible` with the broken condition.
fn update_incompatible(detail: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        UPDATE_INCOMPATIBLE_CODE,
        format!("the integration update is not compatible: {detail}"),
        Some(
            "install the Pohunek release that ships the newer asset set, or uninstall the integration first"
                .to_owned(),
        ),
    )
}

/// `agent_not_installable` for a runtime whose lifecycle the daemon does not
/// run: the shell, a runtime without a handler, and a CLI-run handler.
pub(super) fn not_installable(runtime: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "agent_not_installable",
        format!("{runtime} has no daemon-managed hook integration"),
        None,
    )
}

/// A runtime resolved to the daemon-run handler its definition names.
#[derive(Debug, Clone)]
pub(super) struct Resolved {
    /// The runtime the request addressed; reports are issued for it.
    pub(super) runtime: RuntimeRef,
    /// Identity of the runtime definition, which a host profile names as its
    /// base.
    pub(super) runtime_id: RuntimeId,
    /// Display name of the runtime, used in hints.
    pub(super) display_name: String,
    /// Where the runtime's agent keeps its configuration, when its descriptor
    /// declares it.
    pub(super) config_home: Option<ConfigHome>,
    /// The handler the definition's `integration.handler` selects.
    pub(super) handler: &'static dyn DaemonHandler,
    /// The compiled hook schema the definition names.
    pub(super) schema: &'static HookSchema,
}

/// Resolves the runtime `agent` names to its daemon-run handler.
///
/// # Errors
///
/// `agent_not_installable` for a runtime the host does not serve, one whose
/// definition names no handler, or one whose handler the CLI runs.
pub(super) fn resolve(host: &RuntimeHost, agent: &RuntimeRef) -> Result<Resolved, ProtocolError> {
    let unsupported = || not_installable(agent.as_wire());
    let definition = host.resolve_ref(agent).map_err(|_error| unsupported())?;
    resolved(&definition, agent.clone()).ok_or_else(unsupported)
}

pub(super) fn resolved(
    definition: &crate::agent::host::RuntimeDefinition,
    runtime: RuntimeRef,
) -> Option<Resolved> {
    let id = definition.integration_handler()?;
    let Some(Handler::Daemon(handler)) = handler(id.as_str()) else {
        return None;
    };
    Some(Resolved {
        runtime,
        runtime_id: definition.runtime_id().clone(),
        display_name: definition.display_name().to_owned(),
        config_home: definition.config_home().cloned(),
        handler,
        schema: definition.hook_schema()?,
    })
}

/// Every runtime of the host that names a daemon-run handler.
///
/// Built-in runtimes are visited before package runtimes, each group in runtime
/// id order, so a config home shared by several runtimes is attributed to the
/// built-in runtime that names it.
pub(super) fn managed(host: &RuntimeHost) -> Vec<Resolved> {
    let registry = host.registry();
    let (builtin, packaged): (Vec<_>, Vec<_>) = registry.definitions().partition(|definition| {
        matches!(
            definition.binding().provenance,
            BindingProvenance::Builtin { .. }
        )
    });
    builtin
        .into_iter()
        .chain(packaged)
        .filter_map(|definition| {
            resolved(
                definition,
                RuntimeRef::from_wire(definition.runtime_id().as_str()),
            )
        })
        .collect()
}

/// Stages, verifies, and activates the update of `resolved`'s asset set in
/// `dir`, running `gate` before each committed file.
///
/// The staged set must be compatible with the runtime's current hook schema and
/// with the schema of every retained package version of the same handler.
pub(super) fn update(
    resolved: &Resolved,
    dir: &Path,
    retained: &RetainedSchemas,
    gate: StepGate<'_>,
) -> Result<InstallPaths, ProtocolError> {
    let staged = resolved.handler.stage(dir)?;
    resolved
        .handler
        .verify_compatibility(staged.as_ref(), resolved.schema)?;
    verify_retained(resolved, staged.as_ref(), retained)?;
    staged.activate(gate)
}

/// Proves that `staged` is admitted by every retained schema of the handler
/// and that no retained version is undetermined: its handler is unknown, so it
/// may be the one whose asset set is being replaced, whichever runtime the
/// update addresses.
fn verify_retained(
    resolved: &Resolved,
    staged: &dyn StagedUpdate,
    retained: &RetainedSchemas,
) -> Result<(), ProtocolError> {
    if retained.has_unresolvable() {
        return Err(update_incompatible(
            "a package version still in use has an integration that cannot be determined, so no handler's asset set can be proven compatible with it",
        ));
    }
    let handler = resolved.handler.id();
    for schema in retained.for_handler(handler) {
        admits(handler, staged.manifest(), schema).map_err(|detail| {
            update_incompatible(&format!("a package version still in use: {detail}"))
        })?;
    }
    Ok(())
}

/// Whether `dir` exists as a directory; a symlink is an error.
fn dir_present(dir: &Path) -> Result<bool, ProtocolError> {
    if config_path_kind(dir) == ConfigPath::Symlink {
        return Err(config_dir_is_symlink(dir));
    }
    Ok(dir.is_dir())
}

/// A gate run before each committed file of one home, told which home it is.
pub(super) type HomeGate<'g> = &'g mut dyn FnMut(&Target, usize, &str) -> Result<(), ProtocolError>;

/// Installs into the home of `target`.
///
/// With `skip_absent` a home whose directory does not exist yet is left alone
/// and reported as `None`; without it the absence is the error of the update.
fn install_target(
    target: &Target,
    retained: &RetainedSchemas,
    skip_absent: bool,
    gate: HomeGate<'_>,
) -> Result<Option<IntegrationInstallReport>, ProtocolError> {
    let dir = target.dir()?;
    if skip_absent && !dir_present(dir)? {
        return Ok(None);
    }
    let paths = update(&target.resolved, dir, retained, &mut |index, name| {
        gate(target, index, name)
    })?;
    let mut installed = report(target.resolved.runtime.clone(), &paths);
    installed.home.clone_from(&target.label);
    Ok(Some(installed))
}

/// The failure record of one home.
fn home_failure(target: &Target, error: ProtocolError) -> IntegrationHomeFailure {
    IntegrationHomeFailure {
        agent: target.resolved.runtime.clone(),
        home: target.label.clone().unwrap_or_default(),
        error,
    }
}

/// Installs the hooks of the selected runtime(s) into the selected config
/// homes through their handlers.
///
/// `Some(agent)`, or a profile selection, installs the handler its runtime
/// definition names and fails fast if its config dir is absent. `None` installs
/// every daemon-run handler whose config dir exists, and errors only if none
/// are present. [`HomeSelection::All`] installs into every distinct home whose
/// directory exists, as one transaction per home, and reports a failing home in
/// the result while the others still run, so there is no atomicity across
/// homes; it errors only if no home exists.
///
/// # Errors
///
/// `agent_not_installable` for a runtime without a daemon-run handler,
/// `agent_config_dir_missing` when a requested (or, for `None` and
/// [`HomeSelection::All`], every) config dir is absent,
/// `integration_update_incompatible` when the new asset set is not compatible
/// with the active one, with the runtime's hook schema, or with the schema of a
/// retained package version, the typed errors of an unusable profile
/// selection, or any underlying I/O / settings error. With
/// [`HomeSelection::All`] only the selection errors are returned; a home's own
/// failure is part of the result.
pub fn install_in(
    homes: &ConfigHomes,
    agent: Option<&RuntimeRef>,
    selection: &HomeSelection,
    retained: &RetainedSchemas,
) -> Result<IntegrationInstallResult, ProtocolError> {
    install_in_gated(
        homes,
        agent,
        selection,
        retained,
        &mut |_target, _index, _name| Ok(()),
    )
}

/// [`install_in`] running `gate` before each committed file of each home.
pub(super) fn install_in_gated(
    homes: &ConfigHomes,
    agent: Option<&RuntimeRef>,
    selection: &HomeSelection,
    retained: &RetainedSchemas,
    gate: HomeGate<'_>,
) -> Result<IntegrationInstallResult, ProtocolError> {
    let targets = homes.targets(agent, selection)?;
    let per_home = matches!(selection, HomeSelection::All);
    let skip_absent =
        per_home || (agent.is_none() && !matches!(selection, HomeSelection::Profile(_)));
    let mut installed = Vec::new();
    let mut failed = Vec::new();
    let mut looked = Vec::new();
    let mut names = Vec::new();
    for target in &targets {
        match install_target(target, retained, skip_absent, &mut *gate) {
            Ok(Some(report)) => installed.push(report),
            Ok(None) => {
                let dir = target.dir()?.display().to_string();
                if !looked.contains(&dir) {
                    looked.push(dir);
                }
                if !names.contains(&target.resolved.display_name) {
                    names.push(target.resolved.display_name.clone());
                }
            }
            Err(error) if per_home => failed.push(home_failure(target, error)),
            Err(error) => return Err(error),
        }
    }
    if installed.is_empty() && failed.is_empty() && skip_absent {
        return Err(ProtocolError::new(
            ErrorClass::Runtime,
            "agent_config_dir_missing",
            format!(
                "no agent config dir found (looked for {})",
                looked.join(" and ")
            ),
            Some(format!("install {} first", names.join(" or "))),
        ));
    }
    Ok(IntegrationInstallResult { installed, failed })
}

/// Inspects the managed hook files of the selected runtime(s) in the selected
/// config homes without writing anything.
///
/// Runtimes without a daemon-run handler are rejected so callers cannot
/// mistake an empty report for "nothing is installed". Missing config
/// directories, and homes that cannot be resolved, are reported as unavailable
/// rather than treated as errors.
///
/// # Errors
///
/// `agent_not_installable` for a runtime without a daemon-run handler, or the
/// typed errors of an unusable profile selection.
pub fn status_in(
    homes: &ConfigHomes,
    params: IntegrationStatusParams,
) -> Result<IntegrationStatusResult, ProtocolError> {
    let IntegrationStatusParams {
        agent,
        profile,
        all_profiles,
    } = params;
    let selection = HomeSelection::from_params(profile, all_profiles)?;
    let targets = homes.targets(agent.as_ref(), &selection)?;
    Ok(IntegrationStatusResult {
        agents: targets.iter().map(inspect_target).collect(),
    })
}

/// The read-only status of the home of `target`, with the recovery commands in
/// its warnings addressing that home.
fn inspect_target(target: &Target) -> IntegrationAgentStatus {
    let mut status = target
        .resolved
        .handler
        .inspect(&target.resolved.runtime, target.dir_ref());
    scope_status(&mut status, target);
    status.home.clone_from(&target.label);
    status
}

/// Rewrites the recovery commands of `status` that select the runtime to also
/// select the profile whose home `target` is.
pub(super) fn scope_status(status: &mut IntegrationAgentStatus, target: &Target) {
    if let Some(profile) = target.scope_profile() {
        let runtime = target.resolved.runtime.as_wire();
        for warning in &mut status.warnings {
            *warning = scope_text(warning, runtime, profile);
        }
    }
}

/// Rewrites `--agent <runtime>` in `text` to `--agent <runtime> --profile
/// <profile>`, so a recovery command addresses the home the text describes.
pub(super) fn scope_text(text: &str, runtime: &str, profile: &str) -> String {
    let flag = format!("{AGENT_FLAG} {runtime}");
    text.replace(&flag, &format!("{flag} {PROFILE_FLAG} {profile}"))
}
