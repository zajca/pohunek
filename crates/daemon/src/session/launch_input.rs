//! Application of a runtime's per-launch input to a session launch.
//!
//! All three launch paths (create, resume, fork) compose the environment and
//! the root-position arguments here, so the contribution of the runtime's
//! [`LaunchContributor`](crate::integration::launch_input::LaunchContributor)
//! reaches every relaunch and is never part of the frozen resume snapshot.

// Rust guideline compliant 2026-10-27

use std::path::Path;
use std::sync::Arc;

use protocol::{ErrorClass, ProtocolError, RuntimeRef, SessionId};
use tracing::warn;

use super::{validate_session_name, SessionRegistry};
use crate::agent::host::RuntimeDefinition;
use crate::agent::LaunchEnv;
use crate::checkout_trust::CheckoutFacts;
use crate::integration::launch_input::{
    place, LaunchInput, LaunchInputContext, LaunchKind, LAUNCH_INPUT_UNAVAILABLE_CODE,
};
use crate::runtime::environment::effective_variable;
use crate::store::ResumeBinding;

/// What a launch of one session runs: its kind, the arguments it would
/// otherwise run with and the session name it uses.
#[derive(Debug, Clone, Copy)]
pub(super) struct LaunchSubject<'a> {
    /// Create or resume.
    pub(super) kind: LaunchKind,
    /// The arguments the launch would otherwise run with; they are only read.
    pub(super) frozen_args: &'a [String],
    /// The session name this launch uses, if any.
    pub(super) name: Option<&'a str>,
    /// The runtime's trusted-checkout arguments when this launch's checkout
    /// passed the trust predicate, else empty; see
    /// [`SessionRegistry::trusted_checkout_args`].
    pub(super) trusted_args: &'a [String],
}

/// The environment and root-position arguments of one launch.
#[derive(Debug)]
pub(super) struct LaunchInputs {
    /// The full launch environment, contribution included.
    pub(super) env: LaunchEnv,
    /// Arguments that follow the frozen arguments and precede the native
    /// session arguments: the contributor's, then the runtime's `[placement]`
    /// and `[naming]` arguments; empty for a runtime with none of them.
    pub(super) root_args: Vec<String>,
}

impl SessionRegistry {
    /// Replaces the GitHub URL the trusted-checkout predicate fetches a
    /// repository's remote `HEAD` from, for an embedder or a test with no route
    /// to GitHub; the replacement is a local path. The resolver gets the
    /// lower-cased `owner/repo`; `None` is a failed lookup, which trusts nothing.
    pub fn set_checkout_remote_source<F>(&self, resolver: F)
    where
        F: Fn(&str) -> Option<String> + Send + Sync + 'static,
    {
        if let Ok(mut slot) = self.inner.checkout_remote_source.lock() {
            *slot = Some(crate::checkout_trust::RemoteSourceResolver::new(resolver));
        }
    }

    /// Runs `observe` at each step of the trusted-checkout predicate's read of
    /// the declared paths (right before a path is opened, and before a file's
    /// bytes are read) with the absolute path concerned, for an embedder or a
    /// test that must change the checkout at an exact point of the read.
    pub fn set_checkout_read_observer<F>(&self, observe: F)
    where
        F: Fn(crate::checkout_trust::CheckoutReadStep, &Path) + Send + Sync + 'static,
    {
        if let Ok(mut slot) = self.inner.checkout_read_observer.lock() {
            *slot = Some(crate::checkout_trust::ReadObserver::new(observe));
        }
    }

    /// The runtime's `trusted_checkout_args` for a relaunch of `binding`: the
    /// arguments when the relaunch directory is the directory the session was
    /// created in and that checkout is trusted, else empty.
    ///
    /// The predicate runs on the create-time directory and project of
    /// `binding`, never on the current ones, which terminal output and the
    /// focus process can move. A binding without them, or one relaunched in
    /// another directory, gets no arguments and one `checkout_trust.failed`
    /// warning; a runtime without trusted arguments and a host that trusts no
    /// repository stay silent.
    pub(super) async fn trusted_relaunch_args(
        &self,
        definition: &RuntimeDefinition,
        binding: &ResumeBinding,
    ) -> Vec<String> {
        if definition.checkout_trust().is_none()
            || self.inner.config.trusted_repositories.is_empty()
        {
            return Vec::new();
        }
        let reason = match &binding.launch_checkout {
            None => "the session has no recorded launch checkout",
            Some(launch) if launch.cwd.as_os_str() != binding.cwd.as_os_str() => {
                "the relaunch directory is not the launch directory"
            }
            Some(launch) => {
                return self
                    .trusted_checkout_args(definition, &launch.cwd, launch.project_id.as_deref())
                    .await;
            }
        };
        warn!(
            event = "checkout_trust.failed",
            checkout = %binding.cwd.display(),
            reason,
            "the checkout trust predicate could not be evaluated; the checkout is not trusted"
        );
        Vec::new()
    }

    /// The runtime's `trusted_checkout_args` when the checkout at `cwd` is
    /// trusted, else empty.
    ///
    /// A runtime that declares none, a host that trusts no repository and
    /// every error or ambiguity give no arguments. The predicate fetches the
    /// remote `HEAD` into a private repository and reads the declared paths
    /// on disk, so it runs off the async threads. `project_id` names the
    /// project record whose stored origin must be a trusted repository.
    pub(super) async fn trusted_checkout_args(
        &self,
        definition: &RuntimeDefinition,
        cwd: &Path,
        project_id: Option<&str>,
    ) -> Vec<String> {
        let Some(trust) = definition.checkout_trust() else {
            return Vec::new();
        };
        let repositories = self.inner.config.trusted_repositories.clone();
        let (Some(projects), Some(project_id)) = (self.inner.projects.clone(), project_id) else {
            return Vec::new();
        };
        if repositories.is_empty() {
            return Vec::new();
        }
        let Some(scratch_root) = self
            .inner
            .config
            .worker_state_root
            .as_deref()
            .map(|root| root.join(CHECKOUT_TRUST_SCRATCH_DIR))
        else {
            return Vec::new();
        };
        let remote_source = self
            .inner
            .checkout_remote_source
            .lock()
            .ok()
            .and_then(|resolver| resolver.clone());
        let read_observer = self
            .inner
            .checkout_read_observer
            .lock()
            .ok()
            .and_then(|observer| observer.clone());
        let (cwd, project_id, paths) = (
            cwd.to_path_buf(),
            project_id.to_owned(),
            trust.paths().to_vec(),
        );
        let trusted = tokio::task::spawn_blocking(move || {
            let record = match projects.resolve(&project_id) {
                Ok(record) => record,
                Err(error) => {
                    warn!(
                        event = "checkout_trust.project_unavailable",
                        error = %error.msg,
                        "the launch's project record cannot be read; the checkout is not trusted"
                    );
                    return false;
                }
            };
            crate::checkout_trust::is_trusted(&CheckoutFacts {
                cwd: &cwd,
                stored_origin: record.origin_url.as_deref(),
                trusted_repositories: &repositories,
                paths: &paths,
                scratch_root: &scratch_root,
                remote_source: remote_source.as_ref(),
                read_observer: read_observer.as_ref(),
            })
        })
        .await
        .unwrap_or_else(|_join_error| {
            warn!(
                event = "checkout_trust.task_failed",
                "the checkout trust predicate task failed; the checkout is not trusted"
            );
            false
        });
        if trusted {
            trust.args().to_vec()
        } else {
            Vec::new()
        }
    }

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
            trusted_args,
        } = subject;
        let base = self.launch_env(definition, profile_env, agent.clone(), id);
        let mut declared_args = placement_args(definition, frozen_args);
        declared_args.extend(naming_args(definition, name, id));
        declared_args.extend_from_slice(trusted_args);
        let Some(contribution) = self.contribution(definition, &base, id, kind, frozen_args)?
        else {
            return Ok(LaunchInputs {
                env: base,
                root_args: declared_args,
            });
        };
        let Some(state_root) = self.inner.config.worker_state_root.as_deref() else {
            return Err(unavailable_state_root());
        };
        let placed = place(&contribution, state_root, id)?;
        let mut root_args = placed.root_args;
        root_args.extend(declared_args);
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
                            Some(String::from("set the runtime's config home variable to an absolute path, or set HOME")),
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

/// The session name a launch under `name` hands to the agent: the validated
/// name when the runtime declares `[naming]`, the name passes the session-name
/// rule and the arguments do not withhold it, else `None`. This is the name
/// [`naming_args`] renders.
pub(super) fn launch_name(
    definition: Option<&RuntimeDefinition>,
    name: Option<&str>,
) -> Option<Arc<str>> {
    let definition = definition.filter(|definition| definition.declares_naming())?;
    match validate_session_name(name) {
        Ok(Some(name)) => definition.naming_args(&name).map(|_| Arc::from(name)),
        Ok(None) | Err(_) => None,
    }
}

/// The `[placement] keep_local_args` of a launch, none when the runtime
/// declares none or the frozen arguments already hold them.
fn placement_args(definition: &RuntimeDefinition, frozen_args: &[String]) -> Vec<String> {
    definition
        .placement()
        .map_or_else(Vec::new, |placement| placement.launch_args(frozen_args))
}

/// The `[naming]` arguments of a launch of `id` under `name`.
///
/// A stored name that no longer passes the session-name rule omits the group:
/// a record written before the rule existed must still launch. So does a name
/// the arguments withhold (a dash-leading name in the pair form).
fn naming_args(definition: &RuntimeDefinition, name: Option<&str>, id: &SessionId) -> Vec<String> {
    let Some(name) = name else {
        return Vec::new();
    };
    match validate_session_name(Some(name)) {
        Ok(Some(name)) => definition.naming_args(&name).unwrap_or_else(|| {
            warn!(
                session_id = %id.0,
                "the session name starts with a dash, which the agent's naming arguments would read as an option; launching without them"
            );
            Vec::new()
        }),
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

/// Directory below the worker state root that holds the scratch repositories
/// of the trusted-checkout predicate.
const CHECKOUT_TRUST_SCRATCH_DIR: &str = "checkout-trust";

fn unavailable_state_root() -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        LAUNCH_INPUT_UNAVAILABLE_CODE,
        "the runtime needs launch input but the daemon has no worker state directory",
        None,
    )
}
