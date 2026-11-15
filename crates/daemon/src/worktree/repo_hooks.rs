//! Which hook layers a session runs, and what the host-global layer holds.
//!
//! Every lifecycle hook composes two layers: the host-global script
//! `<config_dir>/hooks/<event>` and the in-repo script the checkout carries
//! (`.pohunek/hooks/<event>`, or the legacy `.pohunek/setup` for
//! `post-create`). A session started with `skip_repo_hooks` runs the
//! host-global layer only: the in-repo layer is never resolved for it, so no
//! file, link or path shape of a checkout it did not choose can make a script
//! run. [`check_required_layer`] refuses such a create before anything runs
//! when `hooks.required` names an event only the in-repo layer could run.

// Rust guideline compliant 2026-11-15

use std::path::{Path, PathBuf};

use protocol::{ErrorClass, HostHookScripts, ProtocolError, HOOK_REQUIRED_SKIPPED_CODE};

use super::{error, HookEvent};

/// Whether a session's hooks resolve the in-repo layer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum RepoHookLayer {
    /// The in-repo script runs after the host-global one.
    #[default]
    Run,
    /// Only the host-global script runs; the in-repo layer is never looked up.
    Skip,
}

impl RepoHookLayer {
    /// The layer a session's recorded `skip_repo_hooks` selects.
    pub(crate) fn of_session(skip_repo_hooks: bool) -> Self {
        if skip_repo_hooks {
            Self::Skip
        } else {
            Self::Run
        }
    }
}

/// The host-global script of `event`, when the layer is enabled and the script
/// is a file. The hook runner, the required-hook check and `host.inspect` all
/// read the layer through this one lookup.
pub(super) fn host_hook_script(config_dir: Option<&Path>, event: HookEvent) -> Option<PathBuf> {
    let script = config_dir?.join("hooks").join(event.as_env());
    script.is_file().then_some(script)
}

/// Which events have a host-global script now, for `host.inspect`. A disabled
/// layer (`config_dir` is `None`) reports every event `false`.
pub(crate) fn host_hook_scripts(config_dir: Option<&Path>) -> HostHookScripts {
    let present = |event| host_hook_script(config_dir, event).is_some();
    HostHookScripts {
        pre_create: present(HookEvent::PreCreate),
        post_create: present(HookEvent::PostCreate),
        pre_remove: present(HookEvent::PreRemove),
        post_remove: present(HookEvent::PostRemove),
        session_start: present(HookEvent::SessionStart),
        session_stop: present(HookEvent::SessionStop),
        agent_state: present(HookEvent::AgentState),
    }
}

/// Refuses a create whose required hook only the skipped in-repo layer could
/// run: `layer` is [`RepoHookLayer::Skip`], `event` is in `required`, and the
/// host-global layer has no script for it.
///
/// The decision reads the host configuration only, never the checkout, so its
/// outcome cannot depend on the content the switch exists to distrust.
///
/// # Errors
///
/// `configuration/hook_required_skipped` naming the event.
pub(super) fn check_required_layer(
    event: HookEvent,
    required: &[HookEvent],
    layer: RepoHookLayer,
    config_dir: Option<&Path>,
) -> Result<(), ProtocolError> {
    if layer == RepoHookLayer::Skip
        && required.contains(&event)
        && host_hook_script(config_dir, event).is_none()
    {
        return Err(error(
            ErrorClass::Configuration,
            HOOK_REQUIRED_SKIPPED_CODE,
            format!(
                "hooks.required lists {event} and the session skips the in-repo hook layer, \
                 but the host has no {event} hook script",
                event = event.as_env()
            ),
            Some(format!(
                "add the host hook script hooks/{event} in the pohunek config directory, \
                 remove {event} from hooks.required in service.toml, or start the session \
                 without skipping the repository's hooks",
                event = event.as_env()
            )),
        ));
    }
    Ok(())
}
