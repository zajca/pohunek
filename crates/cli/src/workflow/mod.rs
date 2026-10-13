//! Workflow plugins: owner-installed command bundles the CLI manages locally.
//!
//! A workflow plugin is a package archive with a `workflow.toml` manifest
//! ([`manifest`]) instead of a runtime descriptor. The CLI keeps it in a
//! package registry of its own ([`store`]), apart from the runtime packages
//! the daemon serves, so no daemon, worker or protocol code ever reads it.
//! Trust is the archive digest the owner supplies at install time.

// Rust guideline compliant 2026-10-13

pub(crate) mod manifest;
pub(crate) mod store;

use std::io::ErrorKind;

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
        }
    }

    /// Recovery hint shown beneath the error.
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
            | Self::Store(_) => ErrorClass::Runtime,
            _ => ErrorClass::Configuration,
        }
    }
}
