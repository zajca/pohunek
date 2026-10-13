//! `pohunek workflow` plugin store commands.
//!
//! Every subcommand works on this user's local workflow plugin store and never
//! contacts a daemon, so any other effective host is refused before any I/O.
//! Install follows the `pohunek plugin` consent pattern: the CLI verifies the
//! archive against the owner's digest, prints what the plugin declares, and
//! installs nothing until the owner repeats the command with `--yes`.
//!
//! `run` exits with the action's own status (128 plus the signal number when
//! a signal ended it), in both output modes. A run the CLI stopped exits with
//! [`TIMEOUT_EXIT_CODE`], [`OUTPUT_LIMIT_EXIT_CODE`], or 128 plus the
//! signal number for a terminating signal the CLI received; a refusal, and a
//! `--json` run whose stdout is not one complete JSON document, exits with
//! status 1 like every other failed command. With `--json`, stdout carries
//! exactly one envelope: the action's document byte for byte as `ok`, or the
//! error, which never holds output bytes.
//!
//! A plugin's jobs and services run as units of the user's service manager:
//! enabling it installs and starts them, disabling or uninstalling it stops
//! and removes them (see [`crate::workflow::units`]). Each unit runs the
//! hidden `__exec` subcommand ([`crate::workflow::supervised`]).

// Rust guideline compliant 2026-10-13

use std::ffi::OsString;
use std::fmt::Write as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Subcommand, ValueEnum, ValueHint};
use package::registry::{InstallStatus, PackageRecord};
use pohunek_platform::supervisor::PluginUnitKind;
use protocol::{PackageDigest, PackageId, PackageVersion};
use serde::Serialize;

use crate::commands::plugin::{self, print_output, sanitize, summary};
use crate::commands::{render_json, render_json_error_with};
use crate::error::CliError;
use crate::paths::Paths;
use crate::service::Context;
use crate::target::is_local_host;
use crate::workflow::manifest::{resolve_argv, Action as Declared, ArgsMode, Manifest};
use crate::workflow::runner::{run_action, Completed, Invocation, Output};
use crate::workflow::store::{read_candidate, Candidate, Purged, Recorded, Store};
use crate::workflow::supervised::{self, Request};
use crate::workflow::units::{self, Host};
use crate::workflow::{config, Error};

/// Exit status of a run stopped at its deadline: the status GNU `timeout`
/// uses, so scripts already treat it as a time-out.
pub(crate) const TIMEOUT_EXIT_CODE: u8 = 124;

/// Exit status of a run stopped because an output stream passed its cap:
/// the status after [`TIMEOUT_EXIT_CODE`], which no shell convention claims
/// for a command's own failure.
pub(crate) const OUTPUT_LIMIT_EXIT_CODE: u8 = 125;

/// Offset POSIX shells add to a signal number in an exit status.
const SIGNAL_EXIT_BASE: i32 = 128;

/// `pohunek workflow` subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum Action {
    /// List installed workflow plugins.
    List {
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
    },

    /// Show one installed workflow plugin and the actions it declares.
    ///
    /// The installed files are verified first; a plugin whose files changed
    /// after install is reported, not shown.
    Inspect {
        /// Plugin id, as shown by `pohunek workflow list`.
        #[arg(value_name = "PLUGIN")]
        plugin: PackageId,
        /// Emit machine-readable JSON instead of text.
        #[arg(long)]
        json: bool,
    },

    /// Install a workflow plugin archive pinned by its digest.
    ///
    /// The archive is verified against `--sha256` and its `workflow.toml` is
    /// validated; this command then prints every action's argv. Nothing is
    /// installed until you repeat the command with `--yes`.
    Install(InstallArgs),

    /// Allow the plugin's actions to run, and install and start its jobs and
    /// services as units of your service manager.
    ///
    /// A plugin that declares jobs or services needs the `pohunek service`
    /// installation, whose `bin/pohunek` the units run.
    Enable {
        /// Plugin id.
        #[arg(value_name = "PLUGIN")]
        plugin: PackageId,
        /// Emit machine-readable JSON instead of text.
        #[arg(long)]
        json: bool,
    },

    /// Block the plugin's actions from running, and stop and remove its jobs
    /// and services.
    Disable {
        /// Plugin id.
        #[arg(value_name = "PLUGIN")]
        plugin: PackageId,
        /// Emit machine-readable JSON instead of text.
        #[arg(long)]
        json: bool,
    },

    /// Run one action of an installed, enabled workflow plugin.
    ///
    /// The plugin's files are verified first. The action runs without a
    /// shell, in the plugin's files directory, with a minimal environment
    /// and the owner limits of `workflow.toml`; its stdout and stderr stream
    /// here. Exits with the action's status.
    ///
    /// Every argument after ACTION goes to the action verbatim, including
    /// `--json`, `--`, `-h` and empty strings, so pohunek options such as
    /// `--json` and `--host` must come before PLUGIN.
    #[command(override_usage = "pohunek workflow run [OPTIONS] <PLUGIN> <ACTION> [ARGS]...")]
    Run {
        /// Print one JSON envelope whose `ok` is the action's stdout, which
        /// must be exactly one JSON document; stderr still streams here.
        #[arg(long)]
        json: bool,
        /// Plugin id.
        #[arg(value_name = "PLUGIN")]
        plugin: PackageId,
        /// The action name, as shown by `pohunek workflow inspect`, then
        /// extra arguments passed verbatim to an action that declares
        /// `args = "passthrough"`.
        // One trailing vector holds the action and its arguments: clap
        // stops option parsing only once a trailing vector has its first
        // value, so a separate ACTION positional would let `--json`, `-h`
        // or `--` right after it reach pohunek instead of the action.
        #[arg(
            value_names = ["ACTION", "ARGS"],
            required = true,
            num_args = 1..,
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        call: Vec<OsString>,
    },

    /// Remove an installed plugin, after stopping and removing its jobs and
    /// services.
    ///
    /// The plugin's state and config directories are kept unless `--purge`
    /// is given; `--purge` also removes the directories an earlier uninstall
    /// kept.
    Uninstall {
        /// Plugin id.
        #[arg(value_name = "PLUGIN")]
        plugin: PackageId,
        /// Also delete the plugin's state and config directories.
        #[arg(long)]
        purge: bool,
        /// Remove a plugin whose installed files fail verification. Files
        /// that verify are refused: uninstall them without this flag.
        #[arg(long)]
        remove_modified: bool,
        /// Emit machine-readable JSON instead of text.
        #[arg(long)]
        json: bool,
    },

    /// Run one job or service of a plugin; the entry point of its service
    /// manager unit, never typed by hand.
    #[command(name = "__exec", hide = true)]
    Exec {
        /// Plugin id.
        #[arg(value_name = "PLUGIN")]
        plugin: PackageId,
        /// The archive digest the unit was installed for.
        #[arg(value_name = "DIGEST", value_parser = plugin::parse_archive_digest)]
        digest: PackageDigest,
        /// Whether the unit is a job or a service.
        #[arg(value_name = "KIND")]
        kind: UnitKind,
        /// The declared job or service name.
        #[arg(value_name = "NAME")]
        name: String,
    },
}

/// The kind of a unit, as `__exec` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum UnitKind {
    /// A periodic job.
    Job,
    /// A long-running service.
    Service,
}

impl Action {
    /// Whether the subcommand requested `--json` output.
    pub(crate) fn wants_json(&self) -> bool {
        match self {
            Self::List { json }
            | Self::Inspect { json, .. }
            | Self::Enable { json, .. }
            | Self::Disable { json, .. }
            | Self::Uninstall { json, .. }
            | Self::Run { json, .. } => *json,
            Self::Install(args) => args.json,
            Self::Exec { .. } => false,
        }
    }
}

/// Arguments of `pohunek workflow install`.
#[derive(Debug, Clone, Args)]
pub(crate) struct InstallArgs {
    /// Workflow plugin archive to install.
    #[arg(value_name = "ARCHIVE", value_hint = ValueHint::FilePath)]
    archive: PathBuf,
    /// The archive's SHA-256 digest (`sha256:<64 hex>`, or the bare hex
    /// `sha256sum` prints). The plugin is trusted only by this digest.
    #[arg(long, value_name = "DIGEST", value_parser = plugin::parse_archive_digest)]
    sha256: PackageDigest,
    /// Install the plugin disabled.
    #[arg(long)]
    no_enable: bool,
    /// Consent to the install after reviewing what the plugin declares.
    #[arg(long)]
    yes: bool,
    /// Emit machine-readable JSON instead of text.
    #[arg(long)]
    json: bool,
}

/// One installed plugin in `--json` output.
#[derive(Debug, Serialize)]
struct PluginInfo {
    id: PackageId,
    version: PackageVersion,
    digest: PackageDigest,
    enabled: bool,
    installed_at_unix_seconds: u64,
}

impl From<&PackageRecord> for PluginInfo {
    fn from(record: &PackageRecord) -> Self {
        Self {
            id: record.identity().id.clone(),
            version: record.identity().version.clone(),
            digest: record.digest().clone(),
            enabled: record.enabled(),
            installed_at_unix_seconds: record.installed_at_unix_seconds(),
        }
    }
}

/// One declared job with `argv[0]` resolved to an absolute path.
#[derive(Debug, Serialize)]
struct JobInfo {
    name: String,
    argv: Vec<String>,
    every_s: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_ms: Option<u64>,
}

/// One declared service with `argv[0]` resolved to an absolute path.
#[derive(Debug, Serialize)]
struct ServiceInfo {
    name: String,
    argv: Vec<String>,
}

/// One declared action with `argv[0]` resolved to an absolute path.
#[derive(Debug, Serialize)]
struct ActionInfo {
    name: String,
    argv: Vec<String>,
    args: ArgsMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_bytes: Option<u64>,
}

/// What a plugin declares and where its files and directories are.
#[derive(Debug, Serialize)]
struct Details {
    description: String,
    files_dir: PathBuf,
    state_dir: PathBuf,
    config_dir: PathBuf,
    actions: Vec<ActionInfo>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    jobs: Vec<JobInfo>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    services: Vec<ServiceInfo>,
}

#[derive(Debug, Serialize)]
struct Listed {
    plugins: Vec<PluginInfo>,
}

#[derive(Debug, Serialize)]
struct Inspected {
    plugin: PluginInfo,
    #[serde(flatten)]
    details: Details,
}

#[derive(Debug, Serialize)]
struct Installed {
    status: &'static str,
    plugin: PluginInfo,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<&'static str>,
}

/// The result of `enable` and `disable`.
#[derive(Debug, Serialize)]
struct Toggled {
    #[serde(flatten)]
    plugin: PluginInfo,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Uninstalled {
    plugin: Option<PluginInfo>,
    purged: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    purge: Option<PurgeInfo>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
    state_dir: PathBuf,
    config_dir: PathBuf,
}

#[derive(Debug, Serialize)]
struct PurgeInfo {
    state_dir: Purged,
    config_dir: Purged,
}

/// Run one `pohunek workflow` subcommand.
///
/// # Errors
///
/// Returns [`CliError`] when the host is not local, the archive or manifest
/// is refused, consent is missing, the plugin is unknown, or the store fails.
pub(crate) async fn run(action: Action, host: &str) -> Result<ExitCode, CliError> {
    if !is_local_host(host) {
        return Err(Error::LocalOnly {
            host: host.to_owned(),
        }
        .into());
    }
    if let Action::Exec {
        plugin,
        digest,
        kind,
        name,
    } = action
    {
        let request = Request {
            plugin,
            digest,
            kind: match kind {
                UnitKind::Job => PluginUnitKind::Job,
                UnitKind::Service => PluginUnitKind::Service,
            },
            name,
        };
        // A service runs for as long as it lives; handing this worker's
        // scheduler to another thread keeps the runtime delivering the stop
        // signals the trampoline polls for, however few workers the host has.
        // The trampoline resolves the store itself, so every failure it meets
        // is paced like a refusal.
        return Ok(tokio::task::block_in_place(|| {
            supervised::execute(&request)
        }));
    }
    let dirs = Paths::workflow_dirs()?;
    let knobs_path = dirs.knobs.clone();
    let store = Store::new(dirs);
    match action {
        Action::List { json } => {
            let result = Listed {
                plugins: store.list()?.iter().map(PluginInfo::from).collect(),
            };
            print_output(json, &result, || render_list(&result.plugins))?;
        }
        Action::Inspect { plugin, json } => {
            let (record, root, manifest) = store.inspect(&plugin)?;
            let result = Inspected {
                plugin: PluginInfo::from(&record),
                details: details(&store, &manifest, root.files_path()),
            };
            print_output(json, &result, || render_inspect(&result))?;
        }
        Action::Install(args) => install(&store, &args).await?,
        Action::Run { json, plugin, call } => {
            let knobs = config::load(&knobs_path)?;
            let (action, args) = call.split_first().expect("clap requires the action name");
            // A name that is not UTF-8 matches no declared action; it is
            // refused as unknown with its bytes escaped.
            let action = action.to_string_lossy();
            let invocation = Invocation {
                plugin: &plugin,
                action: &action,
                args,
                output: if json { Output::Json } else { Output::Text },
            };
            return run_exit(run_action(&store, &knobs, &invocation), json);
        }
        Action::Enable { plugin, json } => {
            let record = units::enable(&store, &mut Host::resolve_lazily(), &plugin).await?;
            print_toggled(json, &record, Vec::new(), "Enabled")?;
        }
        Action::Disable { plugin, json } => {
            let (record, warnings) =
                units::disable(&store, &mut Host::resolve_lazily(), &plugin).await?;
            print_toggled(json, &record, warnings, "Disabled")?;
        }
        Action::Exec { .. } => unreachable!("the trampoline returned above"),
        Action::Uninstall {
            plugin,
            purge,
            remove_modified,
            json,
        } => {
            let mut warnings = Vec::new();
            // The unit lock spans the unit removal and the record removal;
            // without a store there is nothing of either.
            let lock = store.lock_units()?;
            if lock.is_some() && store.installed(&plugin)?.is_some() {
                units::remove_units(&store, &mut Host::resolve_lazily(), &plugin, &mut warnings)
                    .await?;
            }
            let removal = store.uninstall(&plugin, purge, remove_modified)?;
            drop(lock);
            warnings.extend(removal.warnings);
            let result = Uninstalled {
                plugin: removal.record.as_ref().map(PluginInfo::from),
                purged: removal.purged.is_some(),
                purge: removal.purged.map(|(state, config)| PurgeInfo {
                    state_dir: state,
                    config_dir: config,
                }),
                warnings,
                state_dir: store.state_dir(&plugin),
                config_dir: store.config_dir(&plugin),
            };
            print_output(json, &result, || render_uninstall(&plugin, &result))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The exit status of `workflow run`, printing a `--json` result; a run that
/// ended with a status of its own is reported here with that status, and
/// every other failure goes to the shared error sink.
fn run_exit(result: Result<Completed, Error>, json: bool) -> Result<ExitCode, CliError> {
    let error = match result {
        Ok(Completed::Exited(status)) => {
            return Ok(ExitCode::from(crate::service::exit_code(status)));
        }
        Ok(Completed::Document(document)) => {
            print!("{}", render_json(&*document)?);
            return Ok(ExitCode::SUCCESS);
        }
        Err(error) => error,
    };
    let code = match &error {
        Error::ActionTimeout { .. } => TIMEOUT_EXIT_CODE,
        Error::OutputLimit { .. } => OUTPUT_LIMIT_EXIT_CODE,
        Error::Interrupted { signal, .. } => {
            u8::try_from(SIGNAL_EXIT_BASE + signal).unwrap_or(u8::MAX)
        }
        &Error::ActionFailed { status, .. } => {
            let code = crate::service::exit_code(status);
            if json {
                let failed = Failed {
                    error: CliError::from(error).to_protocol_error(),
                    exit_code: code,
                    signal: status.signal(),
                };
                print!("{}", render_json_error_with(&failed)?);
            } else {
                crate::error::render(&error.into(), false);
            }
            return Ok(ExitCode::from(code));
        }
        _ => return Err(error.into()),
    };
    crate::error::render(&error.into(), json);
    Ok(ExitCode::from(code))
}

/// The `--json` error of an action that failed by itself: the standard error
/// object plus the status the CLI exits with.
#[derive(Debug, Serialize)]
struct Failed {
    #[serde(flatten)]
    error: protocol::ProtocolError,
    /// The action's exit code, or 128 plus the signal number when a signal
    /// ended it; the CLI exits with the same status.
    exit_code: u8,
    /// The signal that ended the action, when one did.
    #[serde(skip_serializing_if = "Option::is_none")]
    signal: Option<i32>,
}

fn print_toggled(
    json: bool,
    record: &PackageRecord,
    warnings: Vec<String>,
    verb: &str,
) -> Result<(), CliError> {
    let result = Toggled {
        plugin: PluginInfo::from(record),
        warnings,
    };
    print_output(json, &result, || {
        let mut text = format!("{verb} {}\n", line(&result.plugin));
        for warning in &result.warnings {
            let _ = writeln!(text, "warning: {warning}");
        }
        text
    })
}

async fn install(store: &Store, args: &InstallArgs) -> Result<(), CliError> {
    let candidate = read_candidate(&args.archive, &args.sha256)?;
    let identity = &candidate.manifest.identity;
    // Unlocked and read-only: it only shapes the review; install checks again
    // under the store lock.
    let recorded = store.recorded(&candidate)?;
    if !args.yes {
        if !args.json {
            print!(
                "{}",
                render_review(store, &candidate, !args.no_enable, recorded)
            );
        }
        return Err(plugin::Error::ConsentRequired {
            verb: "installing workflow plugin",
            summary: summary(&identity.id, &identity.version, &candidate.digest),
        }
        .into());
    }
    let has_units = candidate.manifest.has_units();
    let mut host = Host::resolve_lazily();
    if has_units && !args.no_enable && recorded == Recorded::No {
        // Units need the service installation and jobs within the owner's
        // bounds; refuse before any effect.
        units::check_bounds(&candidate.manifest, &config::load(store.knobs_path())?)?;
        let context = Context::resolve().map_err(|error| Error::ServiceNotInstalled {
            reason: error.to_string(),
        })?;
        units::require_installation(&context)?;
        host = Host::given(context);
    }
    // A plugin with units is recorded disabled and enabled afterwards, so its
    // units start only once it is enabled.
    let (report, warning) = store.install(&candidate, !args.no_enable && !has_units)?;
    let refresh = match report.status {
        InstallStatus::AlreadyInstalled | InstallStatus::RootRestored => report.record.enabled(),
        _ => !args.no_enable,
    };
    let report = if has_units && refresh {
        let record = units::enable(store, &mut host, &identity.id).await?;
        package::registry::InstallReport { record, ..report }
    } else {
        report
    };
    let result = Installed {
        status: match report.status {
            InstallStatus::AlreadyInstalled => "already_installed",
            InstallStatus::RootRestored => "root_restored",
            _ => "installed",
        },
        plugin: PluginInfo::from(&report.record),
        warnings: warning.into_iter().collect(),
    };
    print_output(args.json, &result, || {
        let verb = match report.status {
            InstallStatus::AlreadyInstalled => "Already installed",
            InstallStatus::RootRestored => "Restored the files of",
            _ => "Installed",
        };
        let mut text = format!("{verb} {}\n", line(&result.plugin));
        for warning in &result.warnings {
            let _ = writeln!(text, "warning: {warning}");
        }
        text
    })
}

fn details(store: &Store, manifest: &Manifest, files: &Path) -> Details {
    let id = &manifest.identity.id;
    let jobs = manifest
        .jobs
        .iter()
        .map(|(name, job)| JobInfo {
            name: name.clone(),
            argv: resolve_argv(&job.argv, files),
            every_s: job.every_s.get(),
            timeout_ms: job.timeout_ms.map(u64::from),
        })
        .collect();
    let services = manifest
        .services
        .iter()
        .map(|(name, service)| ServiceInfo {
            name: name.clone(),
            argv: resolve_argv(&service.argv, files),
        })
        .collect();
    Details {
        jobs,
        services,
        description: manifest.description.clone(),
        files_dir: files.to_path_buf(),
        state_dir: store.state_dir(id),
        config_dir: store.config_dir(id),
        actions: manifest
            .actions
            .iter()
            .map(|(name, action): (&String, &Declared)| ActionInfo {
                name: name.clone(),
                argv: action.resolved_argv(files),
                args: action.args,
                timeout_ms: action.timeout_ms.map(u64::from),
                max_output_bytes: action.max_output_bytes.map(u64::from),
            })
            .collect(),
    }
}

fn line(info: &PluginInfo) -> String {
    let state = if info.enabled { "enabled" } else { "disabled" };
    format!(
        "{} {} {state} ({})",
        info.id,
        info.version,
        plugin::short_digest(&info.digest)
    )
}

fn render_list(plugins: &[PluginInfo]) -> String {
    if plugins.is_empty() {
        return "No workflow plugins are installed.\n".to_owned();
    }
    plugins.iter().fold(String::new(), |mut text, info| {
        let _ = writeln!(text, "{}", line(info));
        text
    })
}

/// Text shared by the consent review and `inspect`; package text is escaped.
fn render_details(text: &mut String, details: &Details) {
    let _ = writeln!(text, "  description: {}", sanitize(&details.description));
    let _ = writeln!(text, "  files:       {}", details.files_dir.display());
    let _ = writeln!(text, "  state dir:   {}", details.state_dir.display());
    let _ = writeln!(text, "  config dir:  {}", details.config_dir.display());
    if !details.actions.is_empty() {
        let _ = writeln!(text, "Actions (each runs as you, without a shell):");
    }
    for action in &details.actions {
        let extra = match action.args {
            ArgsMode::None => "refused",
            ArgsMode::Passthrough => "passed through",
        };
        let _ = writeln!(text, "  {}  (extra arguments {extra})", action.name);
        // Debug quoting shows argument boundaries and escapes control characters.
        let _ = writeln!(text, "    argv: {:?}", action.argv);
        if let Some(timeout) = action.timeout_ms {
            let _ = writeln!(text, "    requested timeout: {timeout} ms");
        }
        if let Some(bytes) = action.max_output_bytes {
            let _ = writeln!(text, "    requested output cap: {bytes} bytes");
        }
    }
    if !details.jobs.is_empty() {
        let _ = writeln!(
            text,
            "Jobs (each runs periodically as you, without a shell, while the plugin is enabled):"
        );
    }
    for job in &details.jobs {
        let _ = writeln!(text, "  {}  (every {} s)", job.name, job.every_s);
        let _ = writeln!(text, "    argv: {:?}", job.argv);
        if let Some(timeout) = job.timeout_ms {
            let _ = writeln!(text, "    requested timeout: {timeout} ms");
        }
    }
    if !details.services.is_empty() {
        let _ = writeln!(
            text,
            "Services (each runs continuously as you, without a shell, while the plugin is \
             enabled, and restarts when it exits):"
        );
    }
    for service in &details.services {
        let _ = writeln!(text, "  {}", service.name);
        let _ = writeln!(text, "    argv: {:?}", service.argv);
    }
}

fn render_inspect(result: &Inspected) -> String {
    let mut text = format!("Workflow plugin {}\n", line(&result.plugin));
    let _ = writeln!(text, "  digest:      {}", result.plugin.digest);
    render_details(&mut text, &result.details);
    text
}

fn render_review(store: &Store, candidate: &Candidate, enable: bool, recorded: Recorded) -> String {
    let identity = &candidate.manifest.identity;
    let mut text = format!("Workflow plugin {} {}\n", identity.id, identity.version);
    let _ = writeln!(text, "  digest:      {}", candidate.digest);
    let _ = writeln!(text, "  trust:       explicit digest (pinned by you)");
    let state = match recorded {
        Recorded::Intact => "already installed; nothing changes",
        Recorded::FilesMissing => {
            "already installed, but its files are missing; --yes extracts them again"
        }
        Recorded::FilesModified => {
            "already installed, but its files fail verification; the install will be refused"
        }
        Recorded::No if enable => "installed enabled",
        Recorded::No => "installed disabled",
    };
    let _ = writeln!(text, "  result:      {state}");
    let files = store.files_dir(&candidate.digest);
    render_details(&mut text, &details(store, &candidate.manifest, &files));
    text.push_str(
        "Plugin programs run with your account's full authority; the digest only proves \
         which bytes you approved.\nRepeat the command with --yes to install.\n",
    );
    text
}

fn render_uninstall(id: &PackageId, result: &Uninstalled) -> String {
    let mut text = match &result.plugin {
        Some(plugin) => format!("Uninstalled {}\n", line(plugin)),
        None => format!("Workflow plugin {id} is not installed\n"),
    };
    for warning in &result.warnings {
        let _ = writeln!(text, "warning: {warning}");
    }
    match &result.purge {
        Some(purge) => {
            for (name, path, outcome) in [
                ("state", &result.state_dir, purge.state_dir),
                ("config", &result.config_dir, purge.config_dir),
            ] {
                let done = match outcome {
                    Purged::Removed => "removed",
                    Purged::Absent => "was already absent",
                };
                let _ = writeln!(text, "The {name} directory {} {done}.", path.display());
            }
        }
        None => {
            let _ = writeln!(
                text,
                "Kept {} and {} (remove them with --purge).",
                result.state_dir.display(),
                result.config_dir.display()
            );
        }
    }
    text
}
