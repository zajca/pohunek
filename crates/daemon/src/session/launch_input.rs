//! Application of a runtime's per-launch input to a session launch.
//!
//! All three launch paths (create, resume, fork) compose the environment and
//! the root-position arguments here, so the contribution of the runtime's
//! [`LaunchContributor`](crate::integration::launch_input::LaunchContributor)
//! reaches every relaunch and is never part of the frozen resume snapshot.

// Rust guideline compliant 2026-10-27

use protocol::{ErrorClass, ProtocolError, RuntimeRef, SessionId};
use tracing::warn;

use super::{validate_session_name, SessionRegistry};
use crate::agent::host::RuntimeDefinition;
use crate::agent::LaunchEnv;
use crate::integration::launch_input::{
    place, LaunchInput, LaunchInputContext, LaunchKind, LAUNCH_INPUT_UNAVAILABLE_CODE,
};
use crate::runtime::environment::effective_variable;

/// What a launch of one session runs: its kind, the arguments it would
/// otherwise run with and the session name it uses.
#[derive(Debug, Clone, Copy)]
pub(super) struct LaunchSubject<'a> {
    /// Create, resume or fork.
    pub(super) kind: LaunchKind,
    /// The arguments the launch would otherwise run with; they are only read.
    pub(super) frozen_args: &'a [String],
    /// The session name this launch uses, if any.
    pub(super) name: Option<&'a str>,
}

/// The environment and root-position arguments of one launch.
#[derive(Debug)]
pub(super) struct LaunchInputs {
    /// The full launch environment, contribution included.
    pub(super) env: LaunchEnv,
    /// Arguments that follow the frozen arguments and precede the native
    /// session arguments: the contributor's, then the runtime's `[naming]`
    /// arguments; empty for a runtime with neither.
    pub(super) root_args: Vec<String>,
}

impl SessionRegistry {
    /// Composes the environment and root arguments of a launch of `id`.
    ///
    /// A runtime whose handler has no contributor gets exactly
    /// [`Self::launch_env`] and no contributor arguments. `frozen_args` are
    /// the arguments the launch would otherwise run with; they are only read.
    /// A runtime that declares `[naming] launch_args` gets them after the
    /// contributor's arguments, and none while the subject's name is absent
    /// or fails the session-name rule.
    ///
    /// # Errors
    ///
    /// The contributor's own refusal, `launch_input_invalid` for a
    /// contribution outside core's bounds, and `launch_input_unavailable` when
    /// a private file cannot be written, the registry has no worker state
    /// directory to write it in, or the runtime's declared config home does
    /// not resolve; `worker_initialize_invalid` when the base environment
    /// cannot be built.
    pub(super) fn launch_inputs(
        &self,
        definition: &RuntimeDefinition,
        profile_env: &[(String, String)],
        agent: RuntimeRef,
        id: &SessionId,
        subject: LaunchSubject<'_>,
    ) -> Result<LaunchInputs, ProtocolError> {
        let LaunchSubject {
            kind,
            frozen_args,
            name,
        } = subject;
        let base = self.launch_env(definition, profile_env, agent.clone(), id);
        let naming_args = naming_args(definition, name, id);
        let Some(contribution) = self.contribution(definition, &base, id, kind, frozen_args)?
        else {
            return Ok(LaunchInputs {
                env: base,
                root_args: naming_args,
            });
        };
        let Some(state_root) = self.inner.config.worker_state_root.as_deref() else {
            return Err(unavailable_state_root());
        };
        let placed = place(&contribution, state_root, id)?;
        let mut root_args = placed.root_args;
        root_args.extend(naming_args);
        Ok(LaunchInputs {
            env: self.launch_env_with(definition, profile_env, &placed.env, agent, id),
            root_args,
        })
    }

    /// The names of the variables the runtime's contributor adds to a launch of
    /// `id`, computed without writing any private file.
    ///
    /// # Errors
    ///
    /// As [`Self::launch_inputs`] for the contributor's own refusal and the
    /// unresolved environment.
    pub(super) fn contributed_env_names(
        &self,
        definition: &RuntimeDefinition,
        profile_env: &[(String, String)],
        agent: RuntimeRef,
        id: &SessionId,
        kind: LaunchKind,
        frozen_args: &[String],
    ) -> Result<Vec<String>, ProtocolError> {
        let base = self.launch_env(definition, profile_env, agent, id);
        Ok(self
            .contribution(definition, &base, id, kind, frozen_args)?
            .map_or_else(Vec::new, |input| {
                input.env.into_iter().map(|(name, _)| name).collect()
            }))
    }

    /// The contribution of the runtime's contributor to a launch of `id`, or
    /// `None` for a runtime without one.
    fn contribution(
        &self,
        definition: &RuntimeDefinition,
        base: &LaunchEnv,
        id: &SessionId,
        kind: LaunchKind,
        frozen_args: &[String],
    ) -> Result<Option<LaunchInput>, ProtocolError> {
        let Some(contributor) = crate::integration::launch_contributor(definition) else {
            return Ok(None);
        };
        let Some(state_root) = self.inner.config.worker_state_root.as_deref() else {
            return Err(unavailable_state_root());
        };
        let environment = self.launch_base_environment()?;
        let config_home = match definition.config_home() {
            Some(home) => Some(
                home.resolve(&|name| effective_variable(&environment, base, name))
                    .map_err(|error| {
                        ProtocolError::new(
                            ErrorClass::Runtime,
                            LAUNCH_INPUT_UNAVAILABLE_CODE,
                            format!("the runtime's config home cannot be resolved: {error}"),
                            None,
                        )
                    })?,
            ),
            None => None,
        };
        let effective: Vec<(String, String)> = environment
            .iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .chain(base.iter().cloned())
            .collect();
        contributor
            .contribute(&LaunchInputContext {
                session_id: id,
                kind,
                config_home: config_home.as_deref(),
                env: &effective,
                args: frozen_args,
                private_dir: &state_root.join(&id.0),
            })
            .map(Some)
    }
}

/// The `[naming]` arguments of a launch of `id` under `name`.
///
/// A stored name that no longer passes the session-name rule omits the group:
/// a record written before the rule existed must still launch.
fn naming_args(definition: &RuntimeDefinition, name: Option<&str>, id: &SessionId) -> Vec<String> {
    let Some(name) = name else {
        return Vec::new();
    };
    match validate_session_name(Some(name)) {
        Ok(Some(name)) => definition.naming_args(&name),
        Ok(None) => Vec::new(),
        Err(error) => {
            warn!(
                session_id = %id.0,
                error = %error.msg,
                "the session name is not valid for the agent's naming arguments; launching without them"
            );
            Vec::new()
        }
    }
}

fn unavailable_state_root() -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        LAUNCH_INPUT_UNAVAILABLE_CODE,
        "the runtime needs launch input but the daemon has no worker state directory",
        None,
    )
}
