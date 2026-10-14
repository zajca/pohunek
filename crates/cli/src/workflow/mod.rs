//! Workflow plugins: owner-installed command bundles the CLI manages locally.
//!
//! A workflow plugin is a package archive with a `workflow.toml` manifest
//! ([`manifest`]) instead of a runtime descriptor. The CLI keeps it in a
//! package registry of its own ([`store`]), apart from the runtime packages
//! the daemon serves, so no daemon, worker or protocol code ever reads it.
//! Trust is the archive digest the owner supplies at install time.
//! [`runner`] runs one declared action under the owner limits of [`config`]
//! and appends a record to the plugin's run log ([`runlog`]).
//!
//! Declared jobs and services run as units of the user's service manager:
//! [`units`] installs and removes them when a plugin is enabled, disabled or
//! uninstalled, and every unit runs the hidden `pohunek workflow __exec`
//! trampoline ([`supervised`]), which owns every run decision.

// Rust guideline compliant 2026-10-13

pub(crate) mod config;
pub(crate) mod manifest;
pub(crate) mod runlog;
pub(crate) mod runner;
pub(crate) mod store;
pub(crate) mod supervised;
pub(crate) mod units;

use std::io::ErrorKind;
use std::process::ExitStatus;

use package::verify::VerifyError;
use protocol::ErrorClass;

/// Why a workflow command failed.
///
/// No variant carries an argv value, an environment value or manifest text:
/// manifest faults name a fixed reason, a line number or a validated action
/// name only.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    /// A remote host was selected for a local-only command.
    #[error(
        "workflow commands manage plugins on this machine only; --host {host:?} is not supported"
    )]
    LocalOnly {
        /// The rejected effective host.
        host: String,
    },

    /// The archive file cannot be read, is too large, or is not a canonical
    /// package archive.
    #[error("{path}: not a valid workflow plugin archive: {reason}")]
    ArchiveInvalid {
        /// The archive path as typed.
        path: String,
        /// Content-free reason.
        reason: String,
    },

    /// The archive bytes do not have the digest the owner supplied.
    #[error("the archive does not have the digest given with --sha256; nothing was installed")]
    DigestMismatch,

    /// The archive's `workflow.toml` is missing or invalid.
    #[error("the workflow plugin manifest is invalid: {0}")]
    DescriptorInvalid(#[from] manifest::Invalid),

    /// No installed workflow plugin has the id.
    #[error("no workflow plugin with the id {id} is installed")]
    PluginUnknown {
        /// The requested plugin id.
        id: String,
    },

    /// Another archive of the plugin id is already installed.
    #[error("workflow plugin {id} is already installed from another archive")]
    PluginInstalled {
        /// The plugin id.
        id: String,
    },

    /// The installed plugin root is missing or fails verification.
    #[error("the installed files of workflow plugin {id} fail verification: {cause}")]
    RootInvalid {
        /// The plugin id.
        id: String,
        /// Content-free verification failure.
        cause: VerifyError,
    },

    /// `--remove-modified` was given for installed files that verify or are
    /// missing.
    #[error(
        "the installed files of workflow plugin {id} verify or are missing; nothing was removed"
    )]
    RootIntact {
        /// The plugin id.
        id: String,
    },

    /// An install's record replacement committed without confirmed
    /// durability, and reading the record back did not show it.
    #[error("the store could not confirm whether workflow plugin {id} was recorded")]
    InstallUncertain {
        /// The plugin id.
        id: String,
    },

    /// Another workflow command holds the store lock.
    #[error("another workflow command is changing the plugin store; nothing was changed")]
    Busy,

    /// Purging a plugin's state or config directory did not complete.
    #[error("{}purging the directories of workflow plugin {id} did not complete: {detail}", if *.uninstalled { "the plugin was uninstalled, but " } else { "" })]
    PurgeIncomplete {
        /// The plugin id.
        id: String,
        /// Whether this command removed the plugin's registry record.
        uninstalled: bool,
        /// The outcome for each directory, with where any remaining data is.
        detail: String,
    },

    /// The store, state and config locations overlap.
    #[error("the workflow plugin store, state and config directories overlap: {0}")]
    DirsOverlap(String),

    /// The store could not be read or changed safely.
    #[error("the workflow plugin store failed: {0}")]
    Store(String),

    /// The owner knobs file is unsafe or invalid.
    #[error("{path}: the workflow knobs are invalid: {reason}")]
    ConfigInvalid {
        /// The knobs file path.
        path: String,
        /// Content-free reason: a line, a key and its range, or a cause.
        reason: String,
    },

    /// The plugin is installed disabled.
    #[error("workflow plugin {id} is disabled; nothing was run")]
    PluginDisabled {
        /// The plugin id.
        id: String,
    },

    /// The plugin's manifest declares no action of the name.
    #[error("workflow plugin {id} declares no action {action:?}; nothing was run")]
    ActionUnknown {
        /// The plugin id.
        id: String,
        /// The requested name, quoted with control characters escaped.
        action: String,
    },

    /// Extra arguments were given to an action declared with `args = "none"`.
    #[error("action {action} of workflow plugin {id} accepts no extra arguments; {count} were given and nothing was run")]
    ArgsRefused {
        /// The plugin id.
        id: String,
        /// The declared action name.
        action: String,
        /// How many extra arguments were given; their values are not shown.
        count: usize,
    },

    /// Exactly one of the two session origin markers is set.
    #[error("exactly one of POHUNEK_SESSION_ID and POHUNEK_DAEMON_ID is set; a workflow action runs with both or neither, so nothing was run")]
    OriginIncomplete,

    /// The action declares a deadline above the owner's ceiling.
    #[error("action {action} of workflow plugin {id} declares timeout_ms {requested}, above limits.action_timeout_max_ms {max}; nothing was run")]
    TimeoutRefused {
        /// The plugin id.
        id: String,
        /// The declared action name.
        action: String,
        /// The manifest's `timeout_ms`.
        requested: u64,
        /// The owner's ceiling.
        max: u64,
    },

    /// The run context exceeds the owner's cap.
    #[error("the run context of workflow plugin {id} has {bytes} bytes, above limits.max_context_bytes {max}; nothing was run")]
    ContextTooLarge {
        /// The plugin id.
        id: String,
        /// Size of the context JSON.
        bytes: usize,
        /// The owner's cap.
        max: u64,
    },

    /// The plugin's run log cannot be opened or written.
    #[error("the workflow plugin run log failed: {detail}")]
    RunLog {
        /// Path and cause; never record content.
        detail: String,
    },

    /// The action's process could not be started.
    #[error("action {action} of workflow plugin {id} could not be started: {cause}")]
    SpawnFailed {
        /// The plugin id.
        id: String,
        /// The declared action name.
        action: String,
        /// The operating system's error kind.
        cause: ErrorKind,
    },

    /// The runner could not observe the action or read its output.
    #[error("action {action} of workflow plugin {id} could not be observed; its process group was killed")]
    RunFailed {
        /// The plugin id.
        id: String,
        /// The declared action name.
        action: String,
    },

    /// The action did not finish before its deadline.
    #[error("action {action} of workflow plugin {id} did not finish within {timeout_ms} ms and was stopped")]
    ActionTimeout {
        /// The plugin id.
        id: String,
        /// The declared action name.
        action: String,
        /// The deadline that applied.
        timeout_ms: u64,
    },

    /// An output stream of the action passed its cap.
    #[error("action {action} of workflow plugin {id} wrote more than {limit} bytes to {stream} and was stopped")]
    OutputLimit {
        /// The plugin id.
        id: String,
        /// The declared action name.
        action: String,
        /// `stdout` or `stderr`.
        stream: &'static str,
        /// The cap that applied.
        limit: u64,
    },

    /// A JSON-mode action ended by itself with a non-zero status or by a
    /// signal the CLI did not send.
    #[error("action {action} of workflow plugin {id} {}", failure(*status))]
    ActionFailed {
        /// The plugin id.
        id: String,
        /// The declared action name.
        action: String,
        /// How the child ended.
        status: ExitStatus,
    },

    /// A JSON-mode action exited zero, but a process outside its group held
    /// its stdout open, so the document could not be read to its end.
    #[error("the stdout of action {action} of workflow plugin {id} did not end: a process that left the action's process group still holds it open; nothing was printed")]
    OutputIncomplete {
        /// The plugin id.
        id: String,
        /// The declared action name.
        action: String,
    },

    /// A JSON-mode action exited zero, but its stdout is not exactly one
    /// JSON document.
    #[error("the stdout of action {action} of workflow plugin {id} is not exactly one JSON document: {reason}")]
    OutputInvalid {
        /// The plugin id.
        id: String,
        /// The declared action name.
        action: String,
        /// Content-free reason.
        reason: &'static str,
    },

    /// The plugin declares jobs or services, but no `pohunek service`
    /// installation provides the executable their units run.
    #[error("workflow plugin jobs and services need the pohunek service installation: {reason}")]
    ServiceNotInstalled {
        /// What is missing or does not match, by path or key only.
        reason: String,
    },

    /// A declared job or service is outside the owner's `[jobs]` bounds.
    #[error("{unit} of workflow plugin {id} {reason}; nothing was changed")]
    UnitRefused {
        /// The plugin id.
        id: String,
        /// `job <name>` or `service <name>`.
        unit: String,
        /// The bound it breaks, naming the knob.
        reason: String,
    },

    /// The service manager did not complete a unit operation.
    #[error("the service manager units of workflow plugin {id} failed: {detail}")]
    Units {
        /// The plugin id.
        id: String,
        /// What was attempted, what failed, and what was rolled back.
        detail: String,
    },

    /// A fresh install recorded the plugin, but enabling it failed after
    /// that; the plugin stays installed disabled with no units, unless
    /// undoing the failed enable failed too.
    #[error("workflow plugin {id} was installed, but enabling it failed{}: {source}", if *.undone { ", so it stays installed disabled with no units" } else { ", and undoing that failed too, so it may remain enabled or keep units" })]
    InstalledDisabled {
        /// The plugin id.
        id: String,
        /// Why enabling failed; its code is the `cause` of the `--json`
        /// error.
        source: Box<Error>,
        /// Whether the plugin is disabled with no units.
        undone: bool,
    },

    /// A unit runs another archive than the installed one: residue of an
    /// earlier installation.
    #[error("a unit of workflow plugin {id} names another archive than the installed one; nothing was run")]
    UnitStale {
        /// The plugin id.
        id: String,
    },

    /// The installed manifest declares no job or service of the unit's name.
    #[error("workflow plugin {id} declares no {unit}; nothing was run")]
    UnitUnknown {
        /// The plugin id.
        id: String,
        /// `job <name>` or `service <name>`, the name escaped.
        unit: String,
    },

    /// The CLI received a terminating signal while the action ran.
    #[error("action {action} of workflow plugin {id} was stopped because pohunek received signal {signal}")]
    Interrupted {
        /// The plugin id.
        id: String,
        /// The declared action name.
        action: String,
        /// The received signal number.
        signal: i32,
    },
}

impl Error {
    /// Stable wire error code.
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::LocalOnly { .. } => "workflow_local_only",
            Self::ArchiveInvalid { .. } => "workflow_archive_invalid",
            Self::DigestMismatch => "workflow_digest_mismatch",
            Self::DescriptorInvalid(_) => "workflow_descriptor_invalid",
            Self::PluginUnknown { .. } => "workflow_plugin_unknown",
            Self::PluginInstalled { .. } => "workflow_plugin_installed",
            Self::RootInvalid { .. } => "workflow_root_invalid",
            Self::RootIntact { .. } => "workflow_root_intact",
            Self::InstallUncertain { .. } => "workflow_install_uncertain",
            Self::Busy => "workflow_store_busy",
            Self::PurgeIncomplete { .. } => "workflow_purge_incomplete",
            Self::DirsOverlap(_) => "workflow_dirs_overlap",
            Self::Store(_) => "workflow_store_failed",
            Self::ConfigInvalid { .. } => "workflow_config_invalid",
            Self::PluginDisabled { .. } => "workflow_plugin_disabled",
            Self::ActionUnknown { .. } => "workflow_action_unknown",
            Self::ArgsRefused { .. } => "workflow_action_args_refused",
            Self::OriginIncomplete => "workflow_origin_incomplete",
            Self::TimeoutRefused { .. } => "workflow_timeout_refused",
            Self::ContextTooLarge { .. } => "workflow_context_too_large",
            Self::RunLog { .. } => "workflow_run_log_failed",
            Self::SpawnFailed { .. } => "workflow_spawn_failed",
            Self::RunFailed { .. } => "workflow_run_failed",
            Self::ActionTimeout { .. } => "workflow_action_timeout",
            Self::OutputLimit { .. } => "workflow_output_limit",
            Self::Interrupted { .. } => "workflow_action_interrupted",
            Self::ServiceNotInstalled { .. } => "workflow_service_not_installed",
            Self::UnitRefused { .. } => "workflow_unit_refused",
            Self::Units { .. } => "workflow_units_failed",
            Self::UnitStale { .. } => "workflow_unit_stale",
            Self::UnitUnknown { .. } => "workflow_unit_unknown",
            Self::InstalledDisabled { .. } => "workflow_installed_disabled",
            Self::ActionFailed { .. } => "workflow_action_failed",
            Self::OutputIncomplete { .. } => "workflow_output_incomplete",
            Self::OutputInvalid { .. } => "workflow_output_invalid",
        }
    }

    /// Recovery hint shown beneath the error.
    #[expect(
        clippy::too_many_lines,
        reason = "one flat arm per error variant keeps every recovery hint in one place"
    )]
    pub(crate) fn hint(&self) -> &'static str {
        match self {
            Self::LocalOnly { .. } => "run the command on the machine itself, without --host",
            Self::ArchiveInvalid { .. } => "pass a workflow plugin archive (.tar.zst) file",
            Self::DigestMismatch => {
                "check the digest the plugin author published, for example with `sha256sum`"
            }
            Self::DescriptorInvalid(_) => {
                "fix workflow.toml as described in the workflow plugin guide and rebuild the archive"
            }
            Self::PluginUnknown { .. } => "list installed plugins with `pohunek workflow list`",
            Self::PluginInstalled { .. } => {
                "uninstall the installed version with `pohunek workflow uninstall <id>` first"
            }
            Self::RootInvalid {
                cause: VerifyError::RootMissing,
                ..
            } => {
                "the installed files are missing; install the same archive again to restore \
                 them, or remove the plugin with a plain `pohunek workflow uninstall <id>`"
            }
            Self::RootInvalid {
                cause: VerifyError::Unreadable {
                    kind: ErrorKind::PermissionDenied,
                },
                ..
            } => {
                "permission to read the installed files was denied; restore their owner-only \
                 permissions and retry, or remove the plugin with \
                 `pohunek workflow uninstall <id> --remove-modified`"
            }
            Self::RootInvalid {
                cause: VerifyError::Unreadable { .. },
                ..
            } => {
                "the installed files could not be read, so nothing was judged or removed; check \
                 that the store is owned by you and readable, then retry"
            }
            Self::RootInvalid { .. } => {
                "the installed files changed after install and are left as found; remove the \
                 plugin with `pohunek workflow uninstall <id> --remove-modified`"
            }
            Self::RootIntact { .. } => {
                "uninstall the plugin without --remove-modified; a plain uninstall also \
                 removes a plugin whose files are missing"
            }
            Self::InstallUncertain { .. } => {
                "check `pohunek workflow list`; the plugin's state and config directories were \
                 kept, and installing the same archive again is safe"
            }
            Self::Busy => "another workflow command is changing the store; retry",
            Self::PurgeIncomplete { .. } => {
                "fix the cause named above, then repeat `pohunek workflow uninstall <id> --purge`"
            }
            Self::DirsOverlap(_) => {
                "point XDG_STATE_HOME and XDG_CONFIG_HOME at locations that do not contain each other"
            }
            Self::Store(_) => {
                "check that the pohunek state and config directories are owned by you and private"
            }
            Self::ConfigInvalid { .. } => {
                "fix workflow.toml as described in the workflow plugin guide, or remove it to use \
                 the defaults"
            }
            Self::PluginDisabled { .. } => "enable it with `pohunek workflow enable <id>`",
            Self::ActionUnknown { .. } => {
                "list the plugin's actions with `pohunek workflow inspect <id>`"
            }
            Self::ArgsRefused { .. } => {
                "run the action without extra arguments; it declares `args = \"none\"`"
            }
            Self::OriginIncomplete => {
                "set both POHUNEK_SESSION_ID and POHUNEK_DAEMON_ID or neither; inside a managed \
                 session restore the missing marker, never unset the other"
            }
            Self::TimeoutRefused { .. } => {
                "raise limits.action_timeout_max_ms in workflow.toml, or ask the plugin author \
                 for a shorter timeout_ms"
            }
            Self::ContextTooLarge { .. } => "raise limits.max_context_bytes in workflow.toml",
            Self::RunLog { .. } => {
                "check that the plugin state directory and its logs directory are owned by you \
                 and private"
            }
            Self::SpawnFailed { .. } => {
                "check `pohunek workflow inspect <id>`; reinstall the plugin if its files are \
                 missing"
            }
            Self::RunFailed { .. } => "retry the action; the run log records the outcome",
            Self::ActionTimeout { .. } => {
                "the deadline is the action's timeout_ms (up to limits.action_timeout_max_ms) or \
                 limits.action_timeout_default_ms in workflow.toml; raise the one that applied"
            }
            Self::OutputLimit { .. } => {
                "the cap is the lower of the action's max_output_bytes and \
                 limits.max_stdout_bytes / limits.max_stderr_bytes in workflow.toml; raise the \
                 one that applied"
            }
            Self::Interrupted { .. } => "run the action again when it may finish",
            Self::ServiceNotInstalled { .. } => {
                "install the service with `pohunek service install`, or install the plugin \
                 with --no-enable"
            }
            Self::UnitRefused { .. } => {
                "raise the named knob in workflow.toml, or ask the plugin author for a value \
                 within it"
            }
            Self::Units { .. } => {
                "check the user service manager (`systemctl --user` or `launchctl`), then repeat \
                 the command; it is safe to repeat"
            }
            Self::UnitStale { .. } => {
                "the unit is left over from another install; remove it with \
                 `pohunek workflow disable <id>`, or `pohunek workflow enable <id>` to replace it"
            }
            Self::UnitUnknown { .. } => {
                "the unit is left over from another install; remove it with \
                 `pohunek workflow disable <id>`"
            }
            Self::InstalledDisabled { undone: true, .. } => {
                "then run `pohunek workflow enable <id>`; installing the plugin again does not \
                 enable it"
            }
            Self::InstalledDisabled { undone: false, .. } => {
                "then run `pohunek workflow disable <id>` until it succeeds, which removes any \
                 unit and disables the plugin, and then `pohunek workflow enable <id>`"
            }
            Self::ActionFailed { .. } => {
                "the action reported a failure; its stderr above says why, and its stdout was \
                 not printed"
            }
            Self::OutputIncomplete { .. } => {
                "the action left a process running that keeps its stdout open; ask the plugin \
                 author to close stdout in processes it leaves behind"
            }
            Self::OutputInvalid { .. } => {
                "with --json the action must print exactly one JSON document on stdout; run it \
                 without --json to see what it prints"
            }
        }
    }

    /// The recovery shown beneath the error: [`Error::hint`]. For an
    /// installed-disabled plugin it is prefixed by the fix clause of the
    /// failure that left it disabled ([`enable_fix`] of its `source`), then a
    /// comma.
    pub(crate) fn recovery(&self) -> String {
        match self {
            Self::InstalledDisabled { source, .. } => {
                format!("{}, {}", enable_fix(source), self.hint())
            }
            _ => self.hint().to_owned(),
        }
    }

    /// The code of the failure that left an installed plugin disabled.
    pub(crate) fn cause_code(&self) -> Option<&'static str> {
        match self {
            Self::InstalledDisabled { source, .. } => Some(source.code()),
            _ => None,
        }
    }

    /// Error class: owner input mistakes are configuration, broken local state
    /// is runtime.
    pub(crate) fn class(&self) -> ErrorClass {
        match self {
            Self::RootInvalid { .. }
            | Self::RootIntact { .. }
            | Self::InstallUncertain { .. }
            | Self::Busy
            | Self::PurgeIncomplete { .. }
            | Self::Store(_)
            | Self::RunLog { .. }
            | Self::SpawnFailed { .. }
            | Self::RunFailed { .. }
            | Self::ActionTimeout { .. }
            | Self::OutputLimit { .. }
            | Self::Interrupted { .. }
            | Self::ActionFailed { .. }
            | Self::OutputIncomplete { .. }
            | Self::OutputInvalid { .. }
            | Self::Units { .. }
            | Self::UnitStale { .. }
            | Self::InstalledDisabled { .. } => ErrorClass::Runtime,
            _ => ErrorClass::Configuration,
        }
    }
}

/// What to fix before enabling a plugin again after `cause` stopped its
/// enable: a clause without a step of its own, since the step to repeat is
/// the enable.
fn enable_fix(cause: &Error) -> &'static str {
    match cause {
        Error::Busy => "wait for the other workflow command to finish",
        Error::Units { .. } => "check the user service manager (`systemctl --user` or `launchctl`)",
        Error::RootInvalid { .. } => {
            "check the plugin's installed files with `pohunek workflow inspect <id>`"
        }
        Error::ServiceNotInstalled { .. } => "check the `pohunek service` installation",
        Error::Store(_) => {
            "check that the pohunek state and config directories are owned by you and private"
        }
        _ => "fix the cause above",
    }
}

/// How a failed action ended, for [`Error::ActionFailed`]'s message.
fn failure(status: ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt as _;

    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exited with status {code}"),
        (None, Some(signal)) => format!("was ended by signal {signal}"),
        (None, None) => "ended without an exit status".to_owned(),
    }
}
