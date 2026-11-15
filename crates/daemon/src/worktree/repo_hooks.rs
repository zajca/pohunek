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

use protocol::{
    ErrorClass, HostHookScripts, ProtocolError, GIT_HOOKS_ENV_CONFLICT_CODE,
    HOOK_REQUIRED_SKIPPED_CODE,
};

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

/// The `git -c` setting of a skipped layer.
const GIT_NO_HOOKS_OPTION: &str = "core.hooksPath=/dev/null";
/// The git setting a skipped layer pins.
const GIT_HOOKS_PATH_KEY: &str = "core.hooksPath";
/// The `core.hooksPath` of a skipped layer: git looks a hook up below it and
/// finds none.
const GIT_NO_HOOKS_PATH: &str = "/dev/null";
/// Number of `GIT_CONFIG_KEY_<n>`/`GIT_CONFIG_VALUE_<n>` pairs git reads (git
/// 2.31 or newer, which project detection already needs).
const GIT_CONFIG_COUNT: &str = "GIT_CONFIG_COUNT";
/// Command-scoped settings of `git -c`, which git reads after the
/// `GIT_CONFIG_COUNT` pairs, so one of them would win over the pinned path.
const GIT_CONFIG_PARAMETERS: &str = "GIT_CONFIG_PARAMETERS";

impl RepoHookLayer {
    /// The layer a session's recorded `skip_repo_hooks` selects.
    pub(crate) fn of_session(skip_repo_hooks: bool) -> Self {
        if skip_repo_hooks {
            Self::Skip
        } else {
            Self::Run
        }
    }

    /// The options a daemon `git` command of a session with this layer takes
    /// before its subcommand.
    ///
    /// A skipped layer runs none of git's own hooks either: a relative
    /// `core.hooksPath` (husky style) resolves inside the checkout a command
    /// runs in, so a hook the checkout carries would run as the daemon user.
    /// `-c` outranks every configuration file and `GIT_CONFIG_COUNT`.
    pub(crate) fn git_options(self) -> &'static [&'static str] {
        match self {
            Self::Run => &[],
            Self::Skip => &["-c", GIT_NO_HOOKS_OPTION],
        }
    }
}

/// The environment entries that turn git's own hooks off for the agent of a
/// session that skips its checkout's hooks; the agent runs git in that
/// checkout as the daemon user.
///
/// `lookup` reads the environment the entries are appended to. The
/// entries add one `GIT_CONFIG_COUNT` pair behind the pairs already set, so a
/// user's own pairs keep their values and the pinned `core.hooksPath` is the
/// last one git reads. An unset or empty count is zero.
///
/// # Errors
///
/// `configuration/git_hooks_env_conflict` when the environment sets
/// `GIT_CONFIG_PARAMETERS` (git reads it after the pairs, so it could set the
/// path back) or a `GIT_CONFIG_COUNT` that is not a decimal number. The error
/// names the variable, never its value.
pub(crate) fn git_hooks_off_env<V: AsRef<std::ffi::OsStr>>(
    lookup: impl Fn(&str) -> Option<V>,
) -> Result<[(String, String); 3], ProtocolError> {
    let conflict = |detail: &str| {
        error(
            ErrorClass::Configuration,
            GIT_HOOKS_ENV_CONFLICT_CODE,
            format!("the session skips its checkout's hooks, but {detail}"),
            Some(format!(
                "remove {GIT_CONFIG_PARAMETERS} from the agent environment and give \
                 {GIT_CONFIG_COUNT} a decimal value, or start the session without the switch"
            )),
        )
    };
    let set = |name: &str| lookup(name).filter(|value| !value.as_ref().is_empty());
    if set(GIT_CONFIG_PARAMETERS).is_some() {
        return Err(conflict(&format!(
            "the agent environment sets {GIT_CONFIG_PARAMETERS}, which git reads after \
             {GIT_CONFIG_COUNT} and which could turn git's hooks back on"
        )));
    }
    let count = match set(GIT_CONFIG_COUNT) {
        None => 0,
        Some(value) => value
            .as_ref()
            .to_str()
            .filter(|text| text.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|text| text.parse::<usize>().ok())
            .ok_or_else(|| {
                conflict(&format!(
                    "the agent environment sets {GIT_CONFIG_COUNT} to a value that is not a \
                     decimal number"
                ))
            })?,
    };
    if count == usize::MAX {
        return Err(conflict(&format!(
            "the agent environment sets {GIT_CONFIG_COUNT} to a value too large to extend"
        )));
    }
    Ok(git_hooks_off_pair(count))
}

/// The `GIT_CONFIG_COUNT` entries that set `core.hooksPath` to the null device
/// as pair `index`, the last of `index + 1` pairs. A process that starts from
/// an environment without git configuration takes pair 0.
pub(crate) fn git_hooks_off_pair(index: usize) -> [(String, String); 3] {
    [
        (GIT_CONFIG_COUNT.to_owned(), (index + 1).to_string()),
        (
            format!("GIT_CONFIG_KEY_{index}"),
            GIT_HOOKS_PATH_KEY.to_owned(),
        ),
        (
            format!("GIT_CONFIG_VALUE_{index}"),
            GIT_NO_HOOKS_PATH.to_owned(),
        ),
    ]
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
