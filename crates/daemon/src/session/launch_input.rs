//! Application of a runtime's per-launch input to a session launch.
//!
//! All three launch paths (create, resume, fork) compose the environment and
//! the root-position arguments here, so the contribution of the runtime's
//! [`LaunchContributor`](crate::integration::launch_input::LaunchContributor)
//! reaches every relaunch and is never part of the frozen resume snapshot.

// Rust guideline compliant 2026-10-27

use protocol::{ErrorClass, ProtocolError, RuntimeRef, SessionId};

use super::SessionRegistry;
use crate::agent::host::RuntimeDefinition;
use crate::agent::LaunchEnv;
use crate::integration::launch_input::{
    place, LaunchInputContext, LaunchKind, LAUNCH_INPUT_UNAVAILABLE_CODE,
};
use crate::runtime::environment::effective_variable;

/// The environment and root-position arguments of one launch.
#[derive(Debug)]
pub(super) struct LaunchInputs {
    /// The full launch environment, contribution included.
    pub(super) env: LaunchEnv,
    /// Arguments that follow the frozen arguments and precede the native
    /// session arguments; empty for a runtime without a contributor.
    pub(super) root_args: Vec<String>,
}

impl SessionRegistry {
    /// Composes the environment and root arguments of a launch of `id`.
    ///
    /// A runtime whose handler has no contributor gets exactly
    /// [`Self::launch_env`] and no arguments. `frozen_args` are the arguments
    /// the launch would otherwise run with; they are only read.
    ///
    /// # Errors
    ///
    /// The contributor's own refusal, `launch_input_invalid` for a
    /// contribution outside core's bounds, and `launch_input_unavailable` when
    /// a private file cannot be written or the registry has no worker state
    /// directory to write it in.
    pub(super) fn launch_inputs(
        &self,
        definition: &RuntimeDefinition,
        profile_env: &[(String, String)],
        agent: RuntimeRef,
        id: &SessionId,
        kind: LaunchKind,
        frozen_args: &[String],
    ) -> Result<LaunchInputs, ProtocolError> {
        let base = self.launch_env(definition, profile_env, agent.clone(), id);
        let Some(contributor) = crate::integration::launch_contributor(definition) else {
            return Ok(LaunchInputs {
                env: base,
                root_args: Vec::new(),
            });
        };
        let Some(state_root) = self.inner.config.worker_state_root.as_deref() else {
            return Err(ProtocolError::new(
                ErrorClass::Runtime,
                LAUNCH_INPUT_UNAVAILABLE_CODE,
                "the runtime needs launch input but the daemon has no worker state directory",
                None,
            ));
        };
        let config_home = definition.config_home().and_then(|home| {
            let environment = self.launch_base_environment().ok()?;
            home.resolve(&|name| effective_variable(&environment, &base, name))
                .ok()
        });
        let contribution = contributor.contribute(&LaunchInputContext {
            session_id: id,
            kind,
            config_home: config_home.as_deref(),
            env: &base,
            args: frozen_args,
            private_dir: &state_root.join(&id.0),
        })?;
        let placed = place(&contribution, state_root, id)?;
        Ok(LaunchInputs {
            env: self.launch_env_with(definition, profile_env, &placed.env, agent, id),
            root_args: placed.root_args,
        })
    }
}
