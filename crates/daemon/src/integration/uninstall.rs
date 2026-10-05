//! Marker-owned removal of the Claude and Codex hook integrations.
//!
//! Removal edits exactly what the installer owns: the exact managed command
//! strings in the provider registration, the managed Codex trust records, and
//! the managed hook scripts that still carry their ownership marker. The
//! registration is edited first so an agent never references a script that is
//! gone, and the whole set is one rollback-capable transaction under the
//! installer lock. Running it again when nothing is installed changes nothing.

use std::path::Path;

use protocol::{
    IntegrationHomeFailure, IntegrationUninstallReport, IntegrationUninstallResult,
    IntegrationUninstallState, ProtocolError, RuntimeRef,
};
use serde_json::Value;
use toml_edit::{DocumentMut, Item};

use super::commit::{self, Action, Committed, Expected, Operation, Outcome, Step, StepGate};
use super::homes::{ConfigHomes, HomeSelection, Target};
use super::{
    apply_trust_moves, claude_notify_hook_commands, codex_managed_hooks,
    codex_notify_hook_commands, config_dir_is_symlink, config_path_kind, hook_command,
    is_owned_trust_record, json_pretty, owned_trust_hashes, parse_json_object_or_empty,
    path_untrusted, remove_owned_command_hooks, settings_invalid, toml_error_summary, trust_rekeys,
    validate_config_dir, CodexManagedHook, ConfigPath, LoadedFile, OwnedTrustHashes, TrustedDir,
    HOOK_ACTION, HOOK_RELEASE_ACTION, NOTIFY_HOOK_INSTALL_NAME, STATE_HOOK_INSTALL_NAME,
    SUBAGENT_START_ACTION, SUBAGENT_STOP_ACTION,
};

/// Marker line an installed Claude hook script carries.
const CLAUDE_OWNERSHIP_MARKER: &str = "# POHUNEK_INTEGRATION_ID=claude";

/// Marker line an installed Codex hook script carries.
const CODEX_OWNERSHIP_MARKER: &str = "# POHUNEK_INTEGRATION_ID=codex";

/// Remove the managed hooks of one runtime from the selected config homes
/// through its integration handler.
///
/// The runtime is always named: removal never widens to other runtimes, so a
/// failure cannot leave one runtime removed and another unreported. A runtime
/// whose config directory is absent, or that holds nothing the handler owns,
/// is reported as `not_installed` and left unchanged. [`HomeSelection::All`]
/// runs one transaction per distinct home of the runtime and reports a failing
/// home in the result while the others still run, so there is no atomicity
/// across homes.
///
/// # Errors
///
/// `agent_not_installable` for a runtime without a daemon-run handler, the
/// typed errors of an unusable profile selection, a typed configuration error
/// for an unresolvable or unsafe config path (including a symlink), the
/// transaction errors of [`super::commit`], or any underlying I/O or settings
/// error. With [`HomeSelection::All`] only the selection errors are returned.
pub fn uninstall_in(
    homes: &ConfigHomes,
    agent: &RuntimeRef,
    selection: &HomeSelection,
) -> Result<IntegrationUninstallResult, ProtocolError> {
    let per_home = matches!(selection, HomeSelection::All);
    let mut uninstalled = Vec::new();
    let mut failed = Vec::new();
    for target in homes.targets(Some(agent), selection)? {
        match uninstall_target(&target) {
            Ok(report) => uninstalled.push(report),
            Err(error) if per_home => failed.push(IntegrationHomeFailure {
                agent: target.resolved.runtime.clone(),
                home: target.label.clone().unwrap_or_default(),
                error,
            }),
            Err(error) => return Err(error),
        }
    }
    Ok(IntegrationUninstallResult {
        uninstalled,
        failed,
    })
}

/// Removes the managed hooks from the home of `target`.
fn uninstall_target(target: &Target) -> Result<IntegrationUninstallReport, ProtocolError> {
    let dir = target.dir()?;
    let mut report = target
        .resolved
        .handler
        .uninstall(&target.resolved.runtime, dir)?;
    report.home.clone_from(&target.label);
    Ok(report)
}

/// [`uninstall_in`] against the built-in runtimes.
#[cfg(test)]
pub fn uninstall(agent: &RuntimeRef) -> Result<IntegrationUninstallResult, ProtocolError> {
    super::uninstall_for(&crate::agent::host::fixture::builtin_host(), agent)
}

/// Remove the managed Claude hooks from `claude_dir`.
///
/// # Errors
///
/// See [`uninstall`].
pub fn uninstall_claude(claude_dir: &Path) -> Result<IntegrationUninstallReport, ProtocolError> {
    uninstall_claude_gated(claude_dir, &mut |_index, _name| Ok(()))
}

/// Remove the managed Codex hooks from `codex_dir`.
///
/// # Errors
///
/// See [`uninstall`].
pub fn uninstall_codex(codex_dir: &Path) -> Result<IntegrationUninstallReport, ProtocolError> {
    uninstall_codex_gated(codex_dir, &mut |_index, _name| Ok(()))
}

fn empty_report(agent: RuntimeRef) -> IntegrationUninstallReport {
    IntegrationUninstallReport {
        agent,
        state: IntegrationUninstallState::NotInstalled,
        removed_paths: Vec::new(),
        updated_paths: Vec::new(),
        preserved_paths: Vec::new(),
        cleanup_incomplete: Vec::new(),
        home: None,
    }
}

/// Whether the config directory exists; a symlink or non-directory is unsafe.
fn config_dir_present(dir: &Path, label: &str) -> Result<bool, ProtocolError> {
    validate_config_dir(dir.to_path_buf(), label)?;
    match config_path_kind(dir) {
        ConfigPath::Directory => Ok(true),
        ConfigPath::Absent => Ok(false),
        ConfigPath::Symlink => Err(config_dir_is_symlink(dir)),
        ConfigPath::Other => Err(path_untrusted(dir, "agent config path is not a directory")),
        ConfigPath::Unreadable => Err(path_untrusted(
            dir,
            "agent config directory could not be inspected",
        )),
    }
}

/// Removes every owned command from a registration document.
///
/// Returns the rewritten document only when something was removed.
fn strip_registration(
    file: &LoadedFile,
    path: &Path,
    owned_commands: &[String],
) -> Result<Option<String>, ProtocolError> {
    let original = parse_json_object_or_empty(Some(&file.content), path)?;
    let mut updated = original.clone();
    if let Some(hooks) = updated.get_mut("hooks").and_then(Value::as_object_mut) {
        remove_owned_command_hooks(hooks, owned_commands);
        if hooks.is_empty() {
            if let Some(root) = updated.as_object_mut() {
                root.remove("hooks");
            }
        }
    }
    if updated == original {
        return Ok(None);
    }
    json_pretty(path, &updated).map(Some)
}

/// Removes the managed Codex trust records from `config.toml`.
///
/// Only records whose hash is that of a managed command are removed, at any
/// position; a user's own hook records stay and follow their hooks to the keys
/// the shifted positions give them (`moves`). Returns the rewritten document
/// only when something was removed. The `[features] hooks` flag is left as the
/// user's setting.
fn strip_codex_trust(
    content: &str,
    path: &Path,
    hooks_path: &Path,
    owned: &OwnedTrustHashes,
    moves: &[(String, String)],
) -> Result<Option<String>, ProtocolError> {
    let mut doc = content.parse::<DocumentMut>().map_err(|error| {
        settings_invalid(
            path,
            &format!(
                "invalid TOML in Codex config.toml: {}",
                toml_error_summary(content, &error)
            ),
        )
    })?;
    let trust_prefix = format!("{}:", hooks_path.display());
    let Some(state) = doc
        .as_table_mut()
        .get_mut("hooks")
        .and_then(Item::as_table_mut)
        .and_then(|hooks| hooks.get_mut("state"))
        .and_then(Item::as_table_mut)
    else {
        return Ok(None);
    };
    let managed: Vec<String> = state
        .iter()
        .filter(|(key, item)| is_owned_trust_record(key, item, &trust_prefix, owned))
        .map(|(key, _item)| key.to_owned())
        .collect();
    let moves_needed = moves.iter().any(|(from, _to)| state.contains_key(from));
    if managed.is_empty() && !moves_needed {
        return Ok(None);
    }
    for key in &managed {
        state.remove(key);
    }
    apply_trust_moves(state, moves, path)?;
    let state_empty = state.is_empty();
    if let Some(hooks) = doc
        .as_table_mut()
        .get_mut("hooks")
        .and_then(Item::as_table_mut)
    {
        if state_empty {
            hooks.remove("state");
        }
        if hooks.is_empty() {
            doc.as_table_mut().remove("hooks");
        }
    }
    Ok(Some(doc.to_string()))
}

/// The trust-record moves that dropping the managed handlers from `hooks.json`
/// requires, so the user's own hooks keep their approval at their new positions.
fn removal_trust_moves(
    hooks_file: Option<&LoadedFile>,
    hooks_path: &Path,
    owned: &[String],
    managed_hooks: &[CodexManagedHook],
) -> Result<Vec<(String, String)>, ProtocolError> {
    let Some(file) = hooks_file else {
        return Ok(Vec::new());
    };
    let before = parse_json_object_or_empty(Some(&file.content), hooks_path)?;
    let before_hooks = before
        .get("hooks")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut after_hooks = before_hooks.clone();
    remove_owned_command_hooks(&mut after_hooks, owned);
    trust_rekeys(hooks_path, &before_hooks, &after_hooks, managed_hooks)
}

/// The Codex inputs that were loaded and need no edit, as verification targets.
fn unchanged_codex_inputs<'a>(
    hooks_file: Option<&'a LoadedFile>,
    config_file: Option<&'a LoadedFile>,
) -> Vec<(&'static str, &'static str, &'a LoadedFile)> {
    let mut unchanged = Vec::new();
    if let Some(file) = hooks_file {
        unchanged.push(("hooks.json", "Codex hooks.json", file));
    }
    if let Some(file) = config_file {
        unchanged.push(("config.toml", "Codex config.toml", file));
    }
    unchanged
}

/// Appends a no-write verification step for each loaded input left unchanged.
fn push_verifications<'a>(
    steps: &mut Vec<Step<'a>>,
    dir: &'a TrustedDir,
    unchanged: &[(&'static str, &'static str, &'a LoadedFile)],
) {
    for (name, label, file) in unchanged {
        steps.push(Step {
            dir,
            name,
            label,
            action: Action::Verify,
            expected: Expected::Loaded(Some(file)),
        });
    }
}

/// Builds the report from the executed steps' outcomes.
fn report(
    agent: RuntimeRef,
    steps: &[Step<'_>],
    committed: &Committed,
    dir: &Path,
    hooks_subdir: Option<&str>,
) -> IntegrationUninstallReport {
    let mut report = empty_report(agent);
    report
        .cleanup_incomplete
        .clone_from(&committed.cleanup_incomplete);
    for (step, outcome) in steps.iter().zip(&committed.outcomes) {
        let base = hooks_subdir.map_or_else(|| dir.to_path_buf(), |sub| dir.join(sub));
        let path = if matches!(step.action, Action::RemoveOwned { .. }) {
            base.join(step.name)
        } else {
            dir.join(step.name)
        };
        let text = path.display().to_string();
        match outcome {
            Outcome::Written => report.updated_paths.push(text),
            Outcome::Removed => report.removed_paths.push(text),
            Outcome::Foreign => report.preserved_paths.push(text),
            Outcome::Absent | Outcome::Verified => {}
        }
    }
    if !report.removed_paths.is_empty() || !report.updated_paths.is_empty() {
        report.state = IntegrationUninstallState::Removed;
    }
    report
}

/// [`uninstall_claude`] with a gate run before each committed file.
pub(super) fn uninstall_claude_gated(
    claude_dir: &Path,
    gate: StepGate<'_>,
) -> Result<IntegrationUninstallReport, ProtocolError> {
    if !config_dir_present(claude_dir, "Claude config directory")? {
        return Ok(empty_report(RuntimeRef::claude()));
    }
    let root = TrustedDir::open(claude_dir, "Claude config directory")?;
    let _lock = root.lock_installer()?;
    let hooks = root.open_child("hooks", "Claude hooks directory")?;
    let hook_path = claude_dir.join("hooks").join(STATE_HOOK_INSTALL_NAME);
    let notify_path = claude_dir.join("hooks").join(NOTIFY_HOOK_INSTALL_NAME);
    let mut owned = vec![
        hook_command(&hook_path, HOOK_ACTION),
        hook_command(&hook_path, HOOK_RELEASE_ACTION),
        hook_command(&hook_path, SUBAGENT_START_ACTION),
        hook_command(&hook_path, SUBAGENT_STOP_ACTION),
    ];
    owned.extend(claude_notify_hook_commands(&notify_path));

    let settings_path = claude_dir.join("settings.json");
    let settings = root.read_optional("settings.json", "Claude settings.json")?;
    let settings_body = settings
        .as_ref()
        .map(|file| strip_registration(file, &settings_path, &owned))
        .transpose()?
        .flatten();

    // A loaded input that needs no edit still decided what to remove, so it is
    // verified before any destructive step and again before the scripts go.
    let unchanged: Vec<(&'static str, &'static str, &LoadedFile)> = settings
        .as_ref()
        .filter(|_| settings_body.is_none())
        .map(|file| ("settings.json", "Claude settings.json", file))
        .into_iter()
        .collect();
    let mut steps = Vec::new();
    push_verifications(&mut steps, &root, &unchanged);
    if let (Some(file), Some(body)) = (settings.as_ref(), settings_body.as_deref()) {
        steps.push(Step {
            dir: &root,
            name: "settings.json",
            label: "Claude settings.json",
            action: Action::Write {
                body,
                mode: file.mode,
            },
            expected: Expected::Loaded(Some(file)),
        });
    }
    push_verifications(&mut steps, &root, &unchanged);
    if let Some(hooks) = hooks.as_ref() {
        for (name, label) in [
            (STATE_HOOK_INSTALL_NAME, "Claude state hook"),
            (NOTIFY_HOOK_INSTALL_NAME, "Claude notification hook"),
        ] {
            steps.push(Step {
                dir: hooks,
                name,
                label,
                action: Action::RemoveOwned {
                    marker: CLAUDE_OWNERSHIP_MARKER,
                },
                expected: Expected::Any,
            });
        }
    }
    let committed = commit::commit(&steps, gate, Operation::Uninstall)?;
    Ok(report(
        RuntimeRef::claude(),
        &steps,
        &committed,
        claude_dir,
        Some("hooks"),
    ))
}

/// [`uninstall_codex`] with a gate run before each committed file.
pub(super) fn uninstall_codex_gated(
    codex_dir: &Path,
    gate: StepGate<'_>,
) -> Result<IntegrationUninstallReport, ProtocolError> {
    if !config_dir_present(codex_dir, "Codex config directory")? {
        return Ok(empty_report(RuntimeRef::codex()));
    }
    let root = TrustedDir::open(codex_dir, "Codex config directory")?;
    let _lock = root.lock_installer()?;
    let hook_path = codex_dir.join(STATE_HOOK_INSTALL_NAME);
    let notify_path = codex_dir.join(NOTIFY_HOOK_INSTALL_NAME);
    let mut owned = vec![
        hook_command(&hook_path, HOOK_ACTION),
        hook_command(&hook_path, SUBAGENT_START_ACTION),
        hook_command(&hook_path, SUBAGENT_STOP_ACTION),
    ];
    owned.extend(codex_notify_hook_commands(&notify_path));

    let hooks_path = codex_dir.join("hooks.json");
    let hooks_file = root.read_optional("hooks.json", "Codex hooks.json")?;
    let hooks_body = hooks_file
        .as_ref()
        .map(|file| strip_registration(file, &hooks_path, &owned))
        .transpose()?
        .flatten();
    let managed_hooks = codex_managed_hooks(&hook_path, &notify_path);
    let owned_hashes = owned_trust_hashes(&managed_hooks)?;
    let trust_moves =
        removal_trust_moves(hooks_file.as_ref(), &hooks_path, &owned, &managed_hooks)?;
    let config_path = codex_dir.join("config.toml");
    let config_file = root.read_optional("config.toml", "Codex config.toml")?;
    let config_body = config_file
        .as_ref()
        .map(|file| {
            strip_codex_trust(
                &file.content,
                &config_path,
                &hooks_path,
                &owned_hashes,
                &trust_moves,
            )
        })
        .transpose()?
        .flatten();

    let unchanged = unchanged_codex_inputs(
        hooks_file.as_ref().filter(|_| hooks_body.is_none()),
        config_file.as_ref().filter(|_| config_body.is_none()),
    );
    let mut steps = Vec::new();
    push_verifications(&mut steps, &root, &unchanged);
    if let (Some(file), Some(body)) = (hooks_file.as_ref(), hooks_body.as_deref()) {
        steps.push(Step {
            dir: &root,
            name: "hooks.json",
            label: "Codex hooks.json",
            action: Action::Write {
                body,
                mode: file.mode,
            },
            expected: Expected::Loaded(Some(file)),
        });
    }
    if let (Some(file), Some(body)) = (config_file.as_ref(), config_body.as_deref()) {
        steps.push(Step {
            dir: &root,
            name: "config.toml",
            label: "Codex config.toml",
            action: Action::Write {
                body,
                mode: file.mode,
            },
            expected: Expected::Loaded(Some(file)),
        });
    }
    push_verifications(&mut steps, &root, &unchanged);
    for (name, label) in [
        (STATE_HOOK_INSTALL_NAME, "Codex state hook"),
        (NOTIFY_HOOK_INSTALL_NAME, "Codex notification hook"),
    ] {
        steps.push(Step {
            dir: &root,
            name,
            label,
            action: Action::RemoveOwned {
                marker: CODEX_OWNERSHIP_MARKER,
            },
            expected: Expected::Any,
        });
    }
    let committed = commit::commit(&steps, gate, Operation::Uninstall)?;
    Ok(report(
        RuntimeRef::codex(),
        &steps,
        &committed,
        codex_dir,
        None,
    ))
}
