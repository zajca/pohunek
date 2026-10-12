//! Launch-time use of a [`RuntimeDefinition`]: command construction, the
//! frozen launch pin and the version-probe validation selected by data.

use protocol::{BindingProvenance, LaunchBinding, ProtocolError};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::claim::is_reserved;
use super::definition::RuntimeDefinition;
use super::handle::RuntimeHost;
use crate::agent::{build_pty_command, LaunchCommand, LaunchOpts, ValidatedLaunchProgram};

/// The runtime identity a persisted launch snapshot was frozen with.
///
/// A snapshot written without a pin is [`LaunchPin::Unpinned`]: nothing proves
/// which definition it was launched from, so it may only resume while its
/// runtime still resolves to a built-in.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LaunchPin {
    /// No recorded launch binding.
    #[default]
    Unpinned,
    /// The binding of the definition the session was launched from. Boxed so
    /// the launch snapshot stays small in the futures that carry it.
    Pinned(Box<LaunchBinding>),
}

impl LaunchPin {
    /// Pins the binding `definition` records for a session launched from it.
    #[must_use]
    pub fn of(definition: &RuntimeDefinition) -> Self {
        Self::Pinned(Box::new(definition.binding().clone()))
    }

    /// Whether no binding was recorded.
    #[must_use]
    pub fn is_unpinned(&self) -> bool {
        matches!(self, Self::Unpinned)
    }

    /// The recorded binding, if any.
    #[must_use]
    pub fn binding(&self) -> Option<&LaunchBinding> {
        match self {
            Self::Unpinned => None,
            Self::Pinned(binding) => Some(binding.as_ref()),
        }
    }
}

impl Serialize for LaunchPin {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.binding().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for LaunchPin {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Option::<LaunchBinding>::deserialize(deserializer)?
            .map_or(Self::Unpinned, |binding| Self::Pinned(Box::new(binding))))
    }
}

/// Builds the PTY command a bare runtime launches with: its program and
/// launch arguments, resolved on `PATH` unless `opts` carries a validated
/// executable.
///
/// # Errors
///
/// Returns `agent_binary_missing` when the program cannot be resolved.
pub(crate) fn launch_command(
    host: &RuntimeHost,
    definition: &RuntimeDefinition,
    opts: &LaunchOpts,
) -> Result<LaunchCommand, ProtocolError> {
    build_pty_command(
        host.launch_program(definition),
        host.launch_args(definition),
        opts,
    )
}

/// Checks that the definition a runtime id resolves to now is allowed to
/// serve a snapshot frozen with `pin`.
///
/// An unpinned snapshot may only be served by a built-in definition. A pinned
/// one needs the same runtime from the same origin: a built-in pin accepts any
/// built-in descriptor revision of the same package (the snapshot itself is
/// frozen), a package pin needs the identical package and archive digest.
///
/// # Errors
///
/// Returns `runtime_served_by_package` when a built-in or unpinned snapshot
/// meets a package-served runtime, and `runtime_not_installed` for every other
/// definition that cannot serve the pin.
pub(crate) fn check_pin(
    pin: &LaunchPin,
    definition: &RuntimeDefinition,
) -> Result<(), ProtocolError> {
    let current = definition.binding();
    let allowed = match pin {
        LaunchPin::Unpinned => matches!(current.provenance, BindingProvenance::Builtin { .. }),
        LaunchPin::Pinned(frozen) => {
            frozen.runtime_id == current.runtime_id
                && same_origin(&frozen.provenance, &current.provenance)
        }
    };
    if allowed {
        return Ok(());
    }
    let built_in_launch = match pin {
        LaunchPin::Unpinned => true,
        LaunchPin::Pinned(frozen) => {
            frozen.runtime_id == current.runtime_id
                && matches!(frozen.provenance, BindingProvenance::Builtin { .. })
        }
    };
    if built_in_launch
        && is_reserved(&current.runtime_id)
        && matches!(current.provenance, BindingProvenance::Package { .. })
    {
        Err(ProtocolError::runtime_served_by_package(
            &current.runtime_id,
        ))
    } else {
        Err(ProtocolError::runtime_not_installed(&current.runtime_id))
    }
}

fn same_origin(frozen: &BindingProvenance, current: &BindingProvenance) -> bool {
    match (frozen, current) {
        (
            BindingProvenance::Builtin { package: left, .. },
            BindingProvenance::Builtin { package: right, .. },
        ) => left.as_ref().map(|package| &package.id) == right.as_ref().map(|package| &package.id),
        (
            BindingProvenance::Package {
                package: left,
                package_digest: left_digest,
            },
            BindingProvenance::Package {
                package: right,
                package_digest: right_digest,
            },
        ) => left == right && left_digest == right_digest,
        _ => false,
    }
}

/// What a launch spawns, which decides what launch validation resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Spawn<'a> {
    /// A bare host-shell launch: it spawns the configured shell command,
    /// which the launch itself resolves.
    HostShell,
    /// A program the launch spawns by name or absolute path: a runtime's own
    /// program, a profile's program (a profile over the shell included) or
    /// the program frozen into a relaunched session.
    Program(&'a str),
}

/// Runs the launch-time executable validation the definition declares.
///
/// Every spawned program must resolve to an executable before anything is
/// created; only a bare host-shell launch ([`Spawn::HostShell`]) is resolved
/// by its launch. A definition without a version probe then launches whatever
/// the program resolves to. A definition naming a
/// version-probe parser resolves the executable once, probes it in the bounded
/// sandbox and returns that exact path only for the pinned release; the probes
/// and their pinned versions are core data in `capabilities`.
///
/// # Errors
///
/// Returns `agent_binary_missing`, naming only the program's file name, when
/// the spawned program does not resolve to an executable, and
/// `agent_runtime_unsupported` when the probe rejects an executable that
/// exists or the definition names a parser this daemon does not provide.
///
/// `launch_path` is the `PATH` the launched agent will see (the daemon's base
/// environment overridden by the profile); the probe runs under it so an
/// interpreter script resolves the interpreter the launch will use.
pub(crate) fn validate_launch_runtime(
    definition: &RuntimeDefinition,
    spawn: Spawn<'_>,
    launch_path: Option<&std::ffi::OsStr>,
) -> Result<Option<ValidatedLaunchProgram>, ProtocolError> {
    crate::capabilities::validate_definition_launch(definition, spawn, launch_path)
}

/// Validate a launch runtime while preserving a shortened-budget timeout.
pub(crate) fn validate_launch_runtime_with_timeout_detailed(
    definition: &RuntimeDefinition,
    spawn: Spawn<'_>,
    launch_path: Option<&std::ffi::OsStr>,
    timeout: std::time::Duration,
) -> Result<Option<ValidatedLaunchProgram>, crate::capabilities::LaunchValidationFailure> {
    crate::capabilities::validate_definition_launch_with_timeout_detailed(
        definition,
        spawn,
        launch_path,
        timeout,
    )
}
