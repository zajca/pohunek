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

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
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

/// The git settings a skipped layer pins, as key and value: no hook directory
/// (git looks a hook up below `/dev/null` and finds none) and no fsmonitor
/// hook. A repository configuration can name either as a path relative to the
/// checkout, which would run a program the checkout carries.
const GIT_PINNED_SETTINGS: [(&str, &str); 2] =
    [("core.hooksPath", "/dev/null"), ("core.fsmonitor", "false")];
/// Number of `GIT_CONFIG_KEY_<n>`/`GIT_CONFIG_VALUE_<n>` pairs git reads (git
/// 2.31 or newer, which project detection already needs).
const GIT_CONFIG_COUNT: &str = "GIT_CONFIG_COUNT";
/// Command-scoped settings of `git -c`, which git reads after the
/// `GIT_CONFIG_COUNT` pairs, so one of them would win over the pinned settings.
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
    /// A skipped layer runs no hook of the hooks directory and no fsmonitor
    /// hook ([`GIT_PINNED_SETTINGS`]): a relative `core.hooksPath` (husky
    /// style) or `core.fsmonitor` resolves inside the checkout a command runs
    /// in, so a program the checkout carries would run as the daemon user.
    /// `-c` outranks every configuration file and `GIT_CONFIG_COUNT`. The
    /// configured hooks and filter drivers are listed per command and turned
    /// off by [`git_programs_off_env`].
    pub(crate) fn git_options(self) -> &'static [&'static str] {
        match self {
            Self::Run => &[],
            Self::Skip => &[
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
            ],
        }
    }

    /// The options a daemon `git diff` of a session with this layer takes: a
    /// skipped layer converts no file through a textconv driver, which the
    /// checkout's `.gitattributes` selects and a repository configuration may
    /// name by a path inside the checkout. External diff drivers are off for
    /// every session diff (`--no-ext-diff`).
    pub(crate) fn diff_options(self) -> &'static [&'static str] {
        match self {
            Self::Run => &[],
            Self::Skip => &["--no-textconv"],
        }
    }
}

/// The `git config` arguments that list every configured filter driver and
/// hook key, NUL-terminated, for [`git_programs_off_env`]. Exit 1 means none
/// is set.
pub(crate) const GIT_PROGRAM_LISTING: [&str; 5] = [
    "config",
    "--name-only",
    "-z",
    "--get-regexp",
    r"^(filter|hook)\.",
];
/// The `git config` arguments that list every configured hook key,
/// NUL-terminated, for [`config_hook_names`]. Exit 1 means none is set.
pub(crate) const CONFIG_HOOK_LISTING: [&str; 5] =
    ["config", "--name-only", "-z", "--get-regexp", r"^hook\."];
/// The settings that turn one filter driver off: git runs no `clean`,
/// `smudge` or `process` command whose value is empty, and a driver that is
/// not `required` passes the content through unchanged instead of failing.
const FILTER_DRIVER_OFF: [(&str, &str); 4] = [
    ("clean", ""),
    ("smudge", ""),
    ("process", ""),
    ("required", "false"),
];
/// The settings that turn one configured hook (`hook.<name>.command`, git
/// 2.54 or newer) off: an empty `event` drops it from every event it was
/// listed for, and `enabled=false` disables it (for a name that is an event,
/// git 2.55 or newer disables every hook of that event). Older git reads no
/// `hook.*` setting.
const CONFIG_HOOK_OFF: [(&str, &str); 2] = [("event", ""), ("enabled", "false")];

/// The distinct `<name>`s of the `<section>.<name>.<variable>` keys in a
/// NUL-terminated `git config --name-only` listing. A name may hold dots: the
/// variable after the last dot never does.
fn listed_names<'a>(listing: &'a [u8], section: &str) -> BTreeSet<&'a [u8]> {
    listing
        .split(|byte| *byte == 0)
        .filter_map(|key| key.strip_prefix(section.as_bytes())?.strip_prefix(b"."))
        .filter_map(|rest| {
            let dot = rest.iter().rposition(|byte| *byte == b'.')?;
            Some(&rest[..dot])
        })
        .collect()
}

/// `<section>.<name>.<variable>` for every listed name and every setting.
fn settings_of<'a>(
    section: &'a str,
    names: &'a BTreeSet<&'a [u8]>,
    settings: &'a [(&'a str, &'a str)],
) -> impl Iterator<Item = (Vec<u8>, &'a str)> + 'a {
    names.iter().flat_map(move |name| {
        settings.iter().map(move |(variable, value)| {
            let mut key = section.as_bytes().to_vec();
            key.push(b'.');
            key.extend_from_slice(name);
            key.push(b'.');
            key.extend_from_slice(variable.as_bytes());
            (key, *value)
        })
    })
}

/// The environment entries that turn off every filter driver and every
/// configured hook `listing` names, for a daemon `git` command of a session
/// that skips its checkout's hooks.
///
/// Both are defined only in git configuration, which may name their program
/// by a path inside the checkout: the checkout's `.gitattributes` selects a
/// filter driver for `status`, `diff` and a checkout, and a configured hook
/// runs from the checkout's top for its events, `core.hooksPath` or not.
/// `listing` is the output of [`GIT_PROGRAM_LISTING`] run like the command
/// itself, so it sees the same configuration. The entries are
/// `GIT_CONFIG_COUNT` pairs, not `-c`, because `-c` splits at the first `=`
/// and a name may hold one. They start at pair 0: the daemon's `git` inherits
/// no git configuration variables.
pub(crate) fn git_programs_off_env(listing: &[u8]) -> Vec<(OsString, OsString)> {
    let filters = listed_names(listing, "filter");
    let hooks = listed_names(listing, "hook");
    let settings: Vec<_> = settings_of("filter", &filters, &FILTER_DRIVER_OFF)
        .chain(settings_of("hook", &hooks, &CONFIG_HOOK_OFF))
        .collect();
    let mut entries = vec![(
        OsString::from(GIT_CONFIG_COUNT),
        OsString::from(settings.len().to_string()),
    )];
    for (index, (key, value)) in settings.into_iter().enumerate() {
        entries.push((
            OsString::from(format!("GIT_CONFIG_KEY_{index}")),
            OsString::from_vec(key),
        ));
        entries.push((
            OsString::from(format!("GIT_CONFIG_VALUE_{index}")),
            OsString::from(value),
        ));
    }
    entries
}

/// The names of the configured hooks in a [`CONFIG_HOOK_LISTING`] output, for
/// the environment of a process the daemon starts but does not wrap.
///
/// # Errors
///
/// The name that is not UTF-8, which no environment entry the worker passes
/// can carry.
pub(crate) fn config_hook_names(listing: &[u8]) -> Result<Vec<String>, String> {
    listed_names(listing, "hook")
        .into_iter()
        .map(|name| {
            String::from_utf8(name.to_vec()).map_err(|_not_utf8| {
                format!(
                    "the git configuration names a hook {:?} that is not UTF-8",
                    String::from_utf8_lossy(name)
                )
            })
        })
        .collect()
}

/// The error of an agent environment in which git's hooks cannot be pinned
/// off: `detail` says why, naming variables or hooks, never a value.
pub(crate) fn git_hooks_env_conflict(detail: &str) -> ProtocolError {
    error(
        ErrorClass::Configuration,
        GIT_HOOKS_ENV_CONFLICT_CODE,
        format!("the session skips its checkout's hooks, but {detail}"),
        Some(format!(
            "remove {GIT_CONFIG_PARAMETERS} from the agent environment, give \
             {GIT_CONFIG_COUNT} a decimal value and keep the git configuration readable, or \
             start the session without the switch"
        )),
    )
}

/// The `GIT_CONFIG_COUNT` the environment `lookup` reads already sets, for
/// the entries that turn git's own hooks off in it.
///
/// # Errors
///
/// `configuration/git_hooks_env_conflict` when the environment sets
/// `GIT_CONFIG_PARAMETERS` (git reads it after the pairs, so it could set the
/// settings back) or a `GIT_CONFIG_COUNT` that is not a decimal number. The
/// error names the variable, never its value.
pub(crate) fn git_config_count<V: AsRef<std::ffi::OsStr>>(
    lookup: impl Fn(&str) -> Option<V>,
) -> Result<usize, ProtocolError> {
    let set = |name: &str| lookup(name).filter(|value| !value.as_ref().is_empty());
    if set(GIT_CONFIG_PARAMETERS).is_some() {
        return Err(git_hooks_env_conflict(&format!(
            "the agent environment sets {GIT_CONFIG_PARAMETERS}, which git reads after \
             {GIT_CONFIG_COUNT} and which could turn git's hooks back on"
        )));
    }
    match set(GIT_CONFIG_COUNT) {
        None => Ok(0),
        Some(value) => value
            .as_ref()
            .to_str()
            .filter(|text| text.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|text| text.parse::<usize>().ok())
            .ok_or_else(|| {
                git_hooks_env_conflict(&format!(
                    "the agent environment sets {GIT_CONFIG_COUNT} to a value that is not a \
                     decimal number"
                ))
            }),
    }
}

/// The `GIT_CONFIG_COUNT` entries that turn git's own hooks and the fsmonitor
/// hook off for a process of a session that skips its checkout's hooks and
/// runs git in that checkout as the daemon user: the agent and the host hook
/// scripts. They set each of [`GIT_PINNED_SETTINGS`], then turn each of
/// `hooks` (the configured hooks that process's git sees) off, as pairs
/// `first`, `first + 1`, ..., the last pairs git reads. The pairs before
/// `first` keep their values. A process that starts from an environment
/// without git configuration takes `first` 0.
///
/// # Errors
///
/// `configuration/git_hooks_env_conflict` when `first` plus the new pairs
/// does not fit a count.
pub(crate) fn git_hooks_off_pairs(
    first: usize,
    hooks: &[String],
) -> Result<Vec<(String, String)>, ProtocolError> {
    let names: BTreeSet<&[u8]> = hooks.iter().map(String::as_bytes).collect();
    let settings: Vec<(String, &str)> = GIT_PINNED_SETTINGS
        .iter()
        .map(|(key, value)| ((*key).to_owned(), *value))
        .chain(
            settings_of("hook", &names, &CONFIG_HOOK_OFF)
                .map(|(key, value)| (String::from_utf8_lossy(&key).into_owned(), value)),
        )
        .collect();
    let total = first.checked_add(settings.len()).ok_or_else(|| {
        git_hooks_env_conflict(&format!(
            "the agent environment sets {GIT_CONFIG_COUNT} to a value too large to extend"
        ))
    })?;
    let mut entries = vec![(GIT_CONFIG_COUNT.to_owned(), total.to_string())];
    for (offset, (key, value)) in settings.into_iter().enumerate() {
        let index = first + offset;
        entries.push((format!("GIT_CONFIG_KEY_{index}"), key));
        entries.push((format!("GIT_CONFIG_VALUE_{index}"), value.to_owned()));
    }
    Ok(entries)
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
