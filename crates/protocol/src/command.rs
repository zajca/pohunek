//! Typed payloads and errors of `command.run`.
//!
//! `command.run` runs one command a package declared in its descriptor, once,
//! with the fixed program and arguments the owner consented to at install. The
//! request names only the package and the command; it carries no argument,
//! environment, working directory or input. The reply is the program's stdout.
//!
//! Every error is fixed text: none carries a path, program output or a
//! caller-supplied value.

// Rust guideline compliant 2026-10-28

use serde::{Deserialize, Serialize};

use crate::{
    CommandName, ErrorClass, ErrorDetails, PackageDigest, PackageId, PackageIdentity, ProcessExit,
    ProtocolError,
};

/// Parameters for `command.run`.
///
/// Unknown fields are refused, so a request cannot smuggle an argument,
/// environment, working directory or input into the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "CommandRunParams.ts"))]
pub struct CommandRunParams {
    /// Package that declares the command.
    pub package: PackageId,
    /// Command to run.
    pub command: CommandName,
    /// Wall-clock limit in milliseconds, 1 to
    /// [`MAX_COMMAND_TIMEOUT_MS`](crate::MAX_COMMAND_TIMEOUT_MS); absent means
    /// the ceiling and a larger value is lowered to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub timeout_ms: Option<u32>,
    /// Largest stdout in bytes, 1 to
    /// [`MAX_COMMAND_OUTPUT_BYTES`](crate::MAX_COMMAND_OUTPUT_BYTES); absent
    /// means the ceiling and a larger value is lowered to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub max_output_bytes: Option<u32>,
}

/// Result of `command.run`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "CommandRunResult.ts"))]
pub struct CommandRunResult {
    /// Id and version of the package version that ran.
    pub package: PackageIdentity,
    /// Command that ran.
    pub command: CommandName,
    /// Digest of the package version that ran.
    pub digest: PackageDigest,
    /// The program's stdout, verbatim UTF-8.
    pub output: String,
    /// Wall-clock duration of the run in milliseconds.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub duration_ms: u64,
}

/// Declares [`CommandErrorKind`] and [`CommandErrorKind::ALL`] from one list,
/// so a kind cannot be declared without being listed.
macro_rules! command_error_kinds {
    ($($(#[$meta:meta])* $variant:ident,)+) => {
        /// Why a `command.run` request failed.
        ///
        /// Each kind is one stable wire error code with fixed text.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[non_exhaustive]
        pub enum CommandErrorKind {
            $($(#[$meta])* $variant,)+
        }

        impl CommandErrorKind {
            /// Every kind this build knows, in declaration order.
            pub const ALL: [Self; [$(Self::$variant),+].len()] = [$(Self::$variant),+];
        }
    };
}

command_error_kinds! {
    /// The daemon does not implement `command.run`. Produced by a client from
    /// the daemon's `method_not_found`, never by a daemon.
    Unsupported,
    /// No version of the package is installed.
    PackageNotInstalled,
    /// Versions are installed but none is selected.
    NotSelected,
    /// The selected version declares no command of that name.
    NotDeclared,
    /// The command is declared `local` and the call arrived over the overlay.
    NotExposed,
    /// The selected version is disabled.
    PackageDisabled,
    /// The command already runs, or the host-wide limit is reached.
    Busy,
    /// The program exited non-zero or was ended by a signal the daemon did not
    /// send.
    Failed,
    /// The deadline passed.
    TimedOut,
    /// Stdout exceeded the cap.
    OutputTooLarge,
    /// Stdout is not valid UTF-8.
    OutputNotUtf8,
    /// The package root fails verification, the run could not be set up, or
    /// the daemon is shutting down.
    Unavailable,
}

impl CommandErrorKind {
    /// The stable wire error code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Unsupported => "command_unsupported",
            Self::PackageNotInstalled => "command_package_not_installed",
            Self::NotSelected => "command_not_selected",
            Self::NotDeclared => "command_not_declared",
            Self::NotExposed => "command_not_exposed",
            Self::PackageDisabled => "command_package_disabled",
            Self::Busy => "command_busy",
            Self::Failed => "command_failed",
            Self::TimedOut => "command_timed_out",
            Self::OutputTooLarge => "command_output_too_large",
            Self::OutputNotUtf8 => "command_output_not_utf8",
            Self::Unavailable => "command_unavailable",
        }
    }

    const fn message(self) -> &'static str {
        match self {
            Self::Unsupported => "the daemon does not support package commands",
            Self::PackageNotInstalled => "the package is not installed",
            Self::NotSelected => "no version of the package is selected",
            Self::NotDeclared => "the selected package version declares no such command",
            Self::NotExposed => "the command is not callable over the overlay",
            Self::PackageDisabled => "the selected package version is disabled",
            Self::Busy => "the command is already running or the host is running its maximum number of commands",
            Self::Failed => "the command did not exit successfully",
            Self::TimedOut => "the command did not finish within its time limit and was stopped",
            Self::OutputTooLarge => "the command wrote more output than allowed and was stopped",
            Self::OutputNotUtf8 => "the command output is not valid UTF-8",
            Self::Unavailable => "the command could not be run on this host",
        }
    }

    const fn recover(self) -> &'static str {
        match self {
            Self::Unsupported => "upgrade the daemon to a release that supports package commands",
            Self::PackageNotInstalled => "install the package on the host",
            Self::NotSelected => "select an installed version of the package",
            Self::NotDeclared => "check the package id and command name",
            Self::NotExposed => {
                "call the command on the host itself, or install a version that exposes it to peers"
            }
            Self::PackageDisabled => "enable the package on the host",
            Self::Busy => "retry in a moment",
            Self::Failed => "run the package's diagnostics on the host",
            Self::TimedOut => "retry, or raise the timeout up to the protocol maximum",
            Self::OutputTooLarge => {
                "raise the output cap up to the protocol maximum, or fix the program"
            }
            Self::OutputNotUtf8 => "fix the program so it writes UTF-8",
            Self::Unavailable => "run `pohunek plugin doctor` on the host and retry",
        }
    }

    /// The broad error class.
    #[must_use]
    pub const fn class(self) -> ErrorClass {
        match self {
            Self::Unsupported => ErrorClass::Daemon,
            _ => ErrorClass::Runtime,
        }
    }
}

impl From<CommandErrorKind> for ProtocolError {
    fn from(kind: CommandErrorKind) -> Self {
        Self::new(
            kind.class(),
            kind.code(),
            kind.message(),
            Some(kind.recover().to_owned()),
        )
    }
}

impl ProtocolError {
    /// The `runtime/command_failed` error with the way the process ended.
    ///
    /// Carries the exit status or the signal as typed `details`, never the
    /// program's output.
    #[must_use]
    pub fn command_failed(exit: ProcessExit) -> Self {
        Self::from(CommandErrorKind::Failed).with_details(ErrorDetails {
            process_exit: Some(exit),
        })
    }
}
