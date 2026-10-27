//! Tests of the integration handler registry, staging, compatibility,
//! activation rollback, update, and runtime-definition dispatch.

// Rust guideline compliant 2026-10-12

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use pohunek_worker_protocol::{hook_schema, hook_schemas, is_known_hook_handler, HookAction};
use protocol::{
    ErrorClass, IntegrationDoctorParams, IntegrationInstallState, IntegrationStatusParams,
    IntegrationUninstallState, ProtocolError, RuntimeRef, EXPECTED_INTEGRATION_VERSION,
};

use super::doctor::doctor_for_with;
use super::handler::{
    handler, handlers, resolve, update, Handler, Resolved, RetainedSchemas,
    UPDATE_INCOMPATIBLE_CODE,
};
use super::tests::{scoped_dir, tree_snapshot, with_config_dirs};
use super::{
    install_claude, install_codex, install_for, install_for_retained, status_for, uninstall_for,
    INSTALL_IN_PROGRESS_CODE, INSTALL_LOCK_NAME, INTEGRATION_VERSION_PREFIX,
    NOTIFY_HOOK_INSTALL_NAME, STATE_HOOK_INSTALL_NAME,
};
use crate::agent::host::fixture::{
    official_host, pi_shaped_integration_host, pi_shaped_integration_host_as,
    pi_shaped_integration_host_with_homes,
};
use crate::agent::host::RuntimeHost;

/// Error code raised by the injected gate failures below.
const INJECTED_CODE: &str = "injected_step_failure";

/// Mask selecting the file-type bits of a Unix mode.
const FILE_TYPE_MASK: u32 = 0o170_000;

/// File-type bits of a directory.
const DIRECTORY_TYPE: u32 = 0o040_000;

/// Most commit steps either handler runs; bounds the failure-injection loop.
const MAX_COMMIT_STEPS: usize = 8;

/// Handler id of the Codex asset set.
const CODEX_ID: &str = "codex-hook-v1";

/// Handler id of the Claude asset set.
const CLAUDE_ID: &str = "claude-hook-v1";

fn injected() -> ProtocolError {
    ProtocolError::new(ErrorClass::Runtime, INJECTED_CODE, "injected failure", None)
}

/// Snapshot of regular files, excluding the installer lock and directories.
fn content_snapshot(root: &Path) -> Vec<(PathBuf, u32, Vec<u8>)> {
    tree_snapshot(root)
        .into_iter()
        .filter(|(path, mode, _content)| {
            path.file_name() != Some(OsStr::new(INSTALL_LOCK_NAME))
                && mode & FILE_TYPE_MASK != DIRECTORY_TYPE
        })
        .collect()
}

/// Snapshot of every entry, directories included, excluding the installer lock.
fn entry_snapshot(root: &Path) -> Vec<(PathBuf, u32, Vec<u8>)> {
    tree_snapshot(root)
        .into_iter()
        .filter(|(path, _mode, _content)| path.file_name() != Some(OsStr::new(INSTALL_LOCK_NAME)))
        .collect()
}

/// The daemon-run handler `agent` resolves to in the official fixture host.
fn official(agent: &RuntimeRef) -> Resolved {
    resolve(&official_host(), agent).expect("a daemon-run handler")
}

fn agents() -> [RuntimeRef; 2] {
    [RuntimeRef::claude(), RuntimeRef::codex()]
}

/// Writes a user-owned provider file so a staged update has something to
/// merge into.
fn write_user_config(agent: &RuntimeRef, dir: &Path) {
    if *agent == RuntimeRef::claude() {
        fs::write(
            dir.join("settings.json"),
            r#"{"model":"user-model","hooks":{"SessionStart":[{"matcher":"*","hooks":[{"type":"command","command":"echo user-start"}]}]}}"#,
        )
        .expect("write user settings");
    } else {
        fs::write(dir.join("config.toml"), "model = \"user-model\"\n").expect("write user config");
    }
}

/// Installs the current asset set of `agent` into `dir`; returns the state
/// hook path.
fn install_current(agent: &RuntimeRef, dir: &Path) -> PathBuf {
    let paths = if *agent == RuntimeRef::claude() {
        install_claude(dir)
    } else {
        install_codex(dir)
    }
    .expect("install the current asset set");
    paths.hook_path
}

/// Removes the state hook's `Stop` registration from Claude's `settings.json`,
/// leaving the registration shape of the asset set before the main-agent
/// subagent snapshot existed.
fn drop_claude_snapshot_registration(dir: &Path, state_hook: &Path) {
    let settings_path = dir.join("settings.json");
    let mut settings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings_path).expect("read settings"))
            .expect("parse settings");
    let snapshot = format!("sh '{}' stop", state_hook.display());
    let stop = settings["hooks"]["Stop"]
        .as_array_mut()
        .expect("Claude Stop entries");
    let registered = stop.len();
    stop.retain(|group| group["hooks"][0]["command"] != snapshot.as_str());
    assert_eq!(
        stop.len(),
        registered - 1,
        "the current set registers the snapshot hook once"
    );
    fs::write(
        &settings_path,
        serde_json::to_string_pretty(&settings).expect("serialize settings"),
    )
    .expect("write settings");
}

/// Rewrites the version marker of both managed scripts next to `state_hook`.
fn set_script_version(state_hook: &Path, version: u32) {
    let notify_hook = state_hook.with_file_name(NOTIFY_HOOK_INSTALL_NAME);
    for script in [state_hook, notify_hook.as_path()] {
        let text = fs::read_to_string(script).expect("read script");
        let current = format!("{INTEGRATION_VERSION_PREFIX}{EXPECTED_INTEGRATION_VERSION}");
        assert!(
            text.contains(&current),
            "{} carries the marker",
            script.display()
        );
        fs::write(
            script,
            text.replace(&current, &format!("{INTEGRATION_VERSION_PREFIX}{version}")),
        )
        .expect("write script");
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "the signature is the activation gate's"
)]
fn ok_gate(_index: usize, _name: &str) -> Result<(), ProtocolError> {
    Ok(())
}

#[test]
fn the_registry_is_a_closed_set_of_unique_ids() {
    let ids: Vec<&str> = handlers().iter().map(|handler| handler.id()).collect();
    assert_eq!(ids, [CODEX_ID, CLAUDE_ID, "hermes-hook-v1", "muse-hook-v1"]);
    for id in &ids {
        assert_eq!(handler(id).map(Handler::id), Some(*id));
    }
    for unknown in [
        "",
        "codex",
        "Codex-Hook-V1",
        "codex-hook-v1 ",
        "codex-hook-v2",
    ] {
        assert!(handler(unknown).is_none(), "{unknown:?} must be refused");
    }
}

#[test]
fn handlers_and_hook_schemas_pair_exactly() {
    for schema in hook_schemas() {
        for id in schema.handlers {
            let registered = handler(id).unwrap_or_else(|| panic!("{id} is not registered"));
            for action in registered.reported_actions() {
                assert!(
                    schema.allows(*action),
                    "{} must admit {action} reported by {id}",
                    schema.id
                );
            }
        }
    }
    for registered in handlers() {
        assert!(
            is_known_hook_handler(registered.id()),
            "{}",
            registered.id()
        );
        let owners = hook_schemas()
            .iter()
            .filter(|schema| schema.supports_handler(registered.id()))
            .count();
        assert_eq!(owners, 1, "{}", registered.id());
    }
}

#[test]
fn the_official_runtimes_resolve_through_their_package_definitions() {
    let host = official_host();
    for (agent, id) in [
        (RuntimeRef::claude(), CLAUDE_ID),
        (RuntimeRef::codex(), CODEX_ID),
    ] {
        let definition = host.resolve_ref(&agent).expect("the runtime is served");
        assert!(
            matches!(
                definition.binding().provenance,
                protocol::BindingProvenance::Package { .. }
            ),
            "{agent:?} is served by its official package"
        );
        let resolved = resolve(&host, &agent).expect("daemon-run handler");
        assert_eq!(resolved.handler.id(), id);
        assert!(resolved.schema.supports_handler(id));
    }
    for agent in [
        RuntimeRef::shell(),
        RuntimeRef::hermes(),
        RuntimeRef::from_wire("pi"),
    ] {
        assert_eq!(
            resolve(&host, &agent).expect_err("not daemon-run").code,
            "agent_not_installable",
            "{agent:?}"
        );
    }
    for definition in host.registry().definitions() {
        if let Some(id) = definition.integration_handler() {
            assert!(handler(id.as_str()).is_some(), "{id:?} is registered");
        }
    }
}

#[test]
fn staging_changes_nothing_and_holds_the_installer_lock() {
    for agent in agents() {
        for installed in [false, true] {
            let dir = scoped_dir("stage");
            write_user_config(&agent, &dir);
            if installed {
                install_current(&agent, &dir);
            }
            let before = entry_snapshot(&dir);
            let resolved = official(&agent);

            let staged = resolved.handler.stage(&dir).expect("stage");
            assert_eq!(
                entry_snapshot(&dir),
                before,
                "{agent:?} installed={installed}"
            );
            let contended = resolved.handler.stage(&dir).expect_err("lock is held");
            assert_eq!(contended.code, INSTALL_IN_PROGRESS_CODE);

            drop(staged);
            assert_eq!(
                entry_snapshot(&dir),
                before,
                "dropping a staged set changes nothing"
            );
            resolved.handler.stage(&dir).expect("the lock is released");
        }
    }
}

#[test]
fn a_schema_that_does_not_admit_the_asset_set_keeps_the_old_set_active() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let foreign = hook_schema("identity-v1").expect("registered");
    let narrowed = pohunek_worker_protocol::HookSchema {
        handlers: &[CODEX_ID, CLAUDE_ID],
        actions: &[HookAction::IdentityReport],
        ..*hook_schema("identity-subagent-v1").expect("registered")
    };
    for agent in agents() {
        let dir = scoped_dir("compat-schema");
        write_user_config(&agent, &dir);
        install_current(&agent, &dir);
        let before = content_snapshot(&dir);
        let resolved = official(&agent);

        let staged = resolved.handler.stage(&dir).expect("stage");
        let not_driven = resolved
            .handler
            .verify_compatibility(staged.as_ref(), foreign)
            .expect_err("the schema is not driven by the handler");
        assert_eq!(not_driven.code, UPDATE_INCOMPATIBLE_CODE);
        assert!(not_driven.msg.contains("identity-v1"), "{not_driven:?}");
        let narrow = resolved
            .handler
            .verify_compatibility(staged.as_ref(), &narrowed)
            .expect_err("the schema does not admit the reported actions");
        assert_eq!(narrow.code, UPDATE_INCOMPATIBLE_CODE);
        resolved
            .handler
            .verify_compatibility(staged.as_ref(), resolved.schema)
            .expect("the definition's own schema admits the set");
        drop(staged);

        assert_eq!(content_snapshot(&dir), before, "{agent:?}");
        let status = with_config_dirs(&dir, &dir, || resolved.handler.inspect(&agent, Ok(&dir)));
        assert_eq!(status.state, IntegrationInstallState::Current);
    }
}

/// The operations of `actions` as sorted wire names, to compare as a set.
fn action_names(actions: &[HookAction]) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = actions.iter().map(|action| action.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    names
}

#[test]
fn the_staged_registration_reports_exactly_the_declared_operations() {
    for agent in agents() {
        for configured in [false, true] {
            let dir = scoped_dir("compat-derived");
            if configured {
                write_user_config(&agent, &dir);
            }
            let resolved = official(&agent);

            let staged = resolved.handler.stage(&dir).expect("stage");

            assert_eq!(
                action_names(&staged.manifest().actions),
                action_names(resolved.handler.reported_actions()),
                "{agent:?} configured={configured}"
            );
        }
    }
}

#[test]
fn the_embedded_scripts_accept_only_arguments_the_registration_table_maps() {
    use super::{
        script_accepted_arguments, script_action, ScriptRole, CLAUDE_HOOK_ASSET,
        CLAUDE_NOTIFY_HOOK_ASSET, CODEX_HOOK_ASSET, CODEX_NOTIFY_HOOK_ASSET,
    };
    let expected = [
        (
            CLAUDE_HOOK_ASSET.as_str(),
            ScriptRole::State,
            vec![
                "prompt-submit",
                "release",
                "session",
                "stop",
                "subagent-start",
                "subagent-stop",
            ],
        ),
        (
            CODEX_HOOK_ASSET.as_str(),
            ScriptRole::State,
            vec!["session", "subagent-start", "subagent-stop"],
        ),
        (
            CLAUDE_NOTIFY_HOOK_ASSET.as_str(),
            ScriptRole::Notify,
            vec!["notification", "stop", "stop_failure"],
        ),
        (
            CODEX_NOTIFY_HOOK_ASSET.as_str(),
            ScriptRole::Notify,
            vec!["permission_request", "stop"],
        ),
    ];
    for (asset, role, arguments) in expected {
        let mut accepted = script_accepted_arguments(asset).expect("a readable action table");
        accepted.sort_unstable();
        assert_eq!(accepted, arguments);
        for argument in accepted {
            assert!(script_action(role, argument).is_some(), "{argument}");
        }
    }
}

#[test]
fn an_unreadable_action_table_is_not_an_empty_one() {
    use super::script_accepted_arguments;
    assert_eq!(script_accepted_arguments(""), None);
    assert_eq!(script_accepted_arguments("action=$1\nexit 0\n"), None);
    assert_eq!(
        script_accepted_arguments("case \"$action\" in\n  *) exit 0 ;;\nesac\n"),
        None
    );
    assert_eq!(
        script_accepted_arguments("case \"$action\" in\n  a|b) ;;\n  *) exit 0 ;;\nesac\n"),
        Some(vec!["a", "b"])
    );
}

#[test]
fn registered_actions_follow_the_registration_and_the_script_table() {
    use super::{registered_actions, ManagedScript, ScriptRole};
    let path = Path::new("/cfg/state.sh");
    let asset = "case \"$action\" in\n  session|release) ;;\n  *) exit 0 ;;\nesac\n";
    let registration = |commands: &[&str]| {
        serde_json::json!({
            "hooks": {"Event": [{"hooks": commands
                .iter()
                .map(|command| serde_json::json!({"type": "command", "command": command}))
                .collect::<Vec<_>>()}]}
        })
    };
    let derive = |registration: &serde_json::Value, asset: &str| {
        registered_actions(
            registration,
            &[ManagedScript {
                path,
                asset,
                role: ScriptRole::State,
            }],
        )
    };

    assert_eq!(
        derive(&registration(&["sh '/cfg/state.sh' session"]), asset).expect("derive"),
        [HookAction::IdentityReport],
        "an accepted argument that is not registered reports nothing"
    );
    assert_eq!(
        derive(
            &registration(&[
                "sh '/cfg/state.sh' release",
                "sh '/cfg/state.sh' session now"
            ]),
            asset
        )
        .expect("derive"),
        [HookAction::IdentityRelease, HookAction::IdentityReport]
    );
    assert!(
        derive(
            &registration(&[
                "sh '/cfg/state.sh' subagent-start",
                "sh '/cfg/other.sh' session",
                "echo sh '/cfg/state.sh' session",
                "sh '/cfg/state.sh'",
                "sh '/cfg/state.sh' bogus",
            ]),
            asset
        )
        .expect("derive")
        .is_empty(),
        "an argument the script ignores, another script, and a bare run report nothing"
    );
    let not_a_command = serde_json::json!({
        "hooks": {"Event": [{"hooks": [{"type": "http", "command": "sh '/cfg/state.sh' session"}]}]}
    });
    assert!(derive(&not_a_command, asset).expect("derive").is_empty());
    assert!(derive(&serde_json::json!({}), asset)
        .expect("derive")
        .is_empty());

    let unmapped = "case \"$action\" in\n  bogus) ;;\nesac\n";
    assert_eq!(
        derive(&registration(&["sh '/cfg/state.sh' bogus"]), unmapped)
            .expect_err("an accepted argument that maps to no operation")
            .code,
        "integration_asset_unreadable"
    );
    assert_eq!(
        derive(&registration(&["sh '/cfg/state.sh' session"]), "exit 0\n")
            .expect_err("a script without an action table")
            .code,
        "integration_asset_unreadable"
    );
}

#[test]
fn a_retained_schema_that_does_not_admit_the_asset_set_keeps_the_old_set_active() {
    let narrowed: &'static pohunek_worker_protocol::HookSchema =
        Box::leak(Box::new(pohunek_worker_protocol::HookSchema {
            actions: &[HookAction::IdentityReport],
            ..*hook_schema("identity-subagent-v1").expect("registered")
        }));
    for agent in agents() {
        let dir = scoped_dir("retained-schema");
        write_user_config(&agent, &dir);
        install_current(&agent, &dir);
        let before = content_snapshot(&dir);
        let resolved = official(&agent);
        let handler_id = resolved.handler.id();

        let mut retained = RetainedSchemas::default();
        retained.push(handler_id.to_owned(), narrowed);
        let refused = update(&resolved, &dir, &retained, &mut ok_gate)
            .expect_err("a session launched under the narrow schema would lose its reports");
        assert_eq!(refused.code, UPDATE_INCOMPATIBLE_CODE, "{agent:?}");
        assert!(refused.msg.contains("still in use"), "{refused:?}");
        assert_eq!(content_snapshot(&dir), before, "{agent:?}");

        let mut other = RetainedSchemas::default();
        other.push("acme-hook-v1".to_owned(), narrowed);
        update(&resolved, &dir, &other, &mut ok_gate)
            .expect("a schema of another handler takes no part");

        let mut same = RetainedSchemas::default();
        same.push(handler_id.to_owned(), resolved.schema);
        update(&resolved, &dir, &same, &mut ok_gate)
            .expect("the runtime's own schema admits the set");
    }
}

#[test]
fn an_undetermined_retained_version_refuses_every_handler_update() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let package = protocol::PackageId::parse("acme.runtime.other").expect("package id");
    let mut undetermined = RetainedSchemas::default();
    undetermined.push_unresolvable(package);
    let pi_host =
        pi_shaped_integration_host(Path::new("/bin/sh"), CLAUDE_ID, "identity-subagent-v1");
    let pi = RuntimeRef::from_wire("pi");

    for agent in agents() {
        let dir = scoped_dir("retained-unresolvable");
        write_user_config(&agent, &dir);
        install_current(&agent, &dir);
        let before = content_snapshot(&dir);
        let resolved = official(&agent);
        let refused = update(&resolved, &dir, &undetermined, &mut ok_gate)
            .expect_err("the unknown version may drive this handler");
        assert_eq!(refused.code, UPDATE_INCOMPATIBLE_CODE, "{agent:?}");
        assert_eq!(content_snapshot(&dir), before, "{agent:?}");
    }

    let claude = scoped_dir("retained-unresolvable-claude");
    let codex = scoped_dir("retained-unresolvable-codex");
    for (host, agent) in [
        (&pi_host, Some(&pi)),
        (&official_host(), Some(&RuntimeRef::claude())),
        (&official_host(), None),
    ] {
        let refused = with_config_dirs(&claude, &codex, || {
            install_for_retained(host, agent, &undetermined)
        })
        .expect_err("an undetermined retained version");
        assert_eq!(refused.code, UPDATE_INCOMPATIBLE_CODE, "{agent:?}");
    }
    assert!(!claude.join("hooks").join(STATE_HOOK_INSTALL_NAME).exists());
}

#[test]
fn an_active_set_newer_than_the_update_is_replaced_by_it() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    for agent in agents() {
        let dir = scoped_dir("compat-newer-active");
        write_user_config(&agent, &dir);
        let state_hook = install_current(&agent, &dir);
        set_script_version(&state_hook, EXPECTED_INTEGRATION_VERSION + 1);
        let resolved = official(&agent);

        let staged = resolved.handler.stage(&dir).expect("stage");
        assert_eq!(
            staged.manifest().active_version,
            Some(EXPECTED_INTEGRATION_VERSION + 1)
        );
        resolved
            .handler
            .verify_compatibility(staged.as_ref(), resolved.schema)
            .expect("a rolled-back release may reinstall its older set");
        staged.activate(&mut ok_gate).expect("activate");

        let status = with_config_dirs(&dir, &dir, || resolved.handler.inspect(&agent, Ok(&dir)));
        assert_eq!(status.state, IntegrationInstallState::Current, "{agent:?}");
        assert_eq!(status.installed_version, Some(EXPECTED_INTEGRATION_VERSION));
    }
}

#[test]
fn a_failing_activation_restores_the_exact_prior_tree() {
    for agent in agents() {
        for existing in [false, true] {
            let mut failures = 0;
            for fail_at in 0..MAX_COMMIT_STEPS {
                let dir = scoped_dir("activate-rollback");
                write_user_config(&agent, &dir);
                if existing {
                    install_current(&agent, &dir);
                }
                let before = entry_snapshot(&dir);
                let resolved = official(&agent);
                let mut gate = |index: usize, _name: &str| {
                    if index == fail_at {
                        Err(injected())
                    } else {
                        Ok(())
                    }
                };

                let outcome = update(&resolved, &dir, &RetainedSchemas::default(), &mut gate);

                let Err(error) = outcome else {
                    break;
                };
                failures += 1;
                assert_eq!(error.code, INJECTED_CODE);
                assert_eq!(
                    entry_snapshot(&dir),
                    before,
                    "{agent:?} existing={existing} fail_at={fail_at} left residue"
                );
            }
            assert!(
                failures >= 3,
                "{agent:?} existing={existing}: {failures} steps"
            );
        }
    }
}

#[test]
fn a_script_body_that_differs_at_the_current_version_is_outdated_and_the_doctor_fails() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    for agent in agents() {
        let dir = scoped_dir("same-version-drift");
        write_user_config(&agent, &dir);
        let state_hook = install_current(&agent, &dir);
        let body = fs::read_to_string(&state_hook).expect("read the installed script");
        assert!(body.contains(&format!(
            "{INTEGRATION_VERSION_PREFIX}{EXPECTED_INTEGRATION_VERSION}"
        )));
        fs::write(&state_hook, format!("{body}\n# edited after install\n"))
            .expect("drift the script body");

        let status = with_config_dirs(&dir, &dir, || {
            official(&agent).handler.inspect(&agent, Ok(&dir))
        });
        assert_eq!(status.state, IntegrationInstallState::Outdated, "{agent:?}");
        assert_eq!(
            status.installed_version,
            Some(EXPECTED_INTEGRATION_VERSION),
            "the version marker alone cannot tell this drift"
        );

        let doctor = with_config_dirs(&dir, &dir, || {
            doctor_for_with(
                &official_host(),
                IntegrationDoctorParams {
                    agent: Some(agent.clone()),
                    ..Default::default()
                },
                &[],
                &[],
            )
        })
        .expect("doctor");
        assert!(!doctor.ok, "{agent:?}");
        let finding = doctor.agents[0]
            .findings
            .iter()
            .find(|finding| finding.code == protocol::IntegrationFindingCode::AssetModified)
            .expect("an asset_modified finding");
        assert_eq!(
            finding.severity,
            protocol::IntegrationFindingSeverity::Error
        );
        assert!(finding
            .remediation
            .as_deref()
            .is_some_and(|text| text.contains("pohunek integration install --agent")));
    }
}

#[test]
fn updating_from_the_previous_asset_set_activates_atomically() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    for agent in agents() {
        let dir = scoped_dir("update-previous");
        write_user_config(&agent, &dir);
        let state_hook = install_current(&agent, &dir);
        let resolved = official(&agent);
        if agent == RuntimeRef::claude() {
            drop_claude_snapshot_registration(&dir, &state_hook);
            let unregistered =
                with_config_dirs(&dir, &dir, || resolved.handler.inspect(&agent, Ok(&dir)));
            assert_ne!(
                unregistered.state,
                IntegrationInstallState::Current,
                "a current script without the snapshot Stop registration is incomplete"
            );
        }
        set_script_version(&state_hook, EXPECTED_INTEGRATION_VERSION - 1);
        let previous = content_snapshot(&dir);
        let outdated = with_config_dirs(&dir, &dir, || resolved.handler.inspect(&agent, Ok(&dir)));
        assert_eq!(outdated.state, IntegrationInstallState::Outdated);
        assert_eq!(
            outdated.installed_version,
            Some(EXPECTED_INTEGRATION_VERSION - 1)
        );

        let staged = resolved.handler.stage(&dir).expect("stage");
        let manifest = staged.manifest().clone();
        assert_eq!(manifest.version, EXPECTED_INTEGRATION_VERSION);
        assert_eq!(
            manifest.active_version,
            Some(EXPECTED_INTEGRATION_VERSION - 1)
        );
        resolved
            .handler
            .verify_compatibility(staged.as_ref(), resolved.schema)
            .expect("the previous set is compatible");
        assert_eq!(
            content_snapshot(&dir),
            previous,
            "the previous set stays active until activation"
        );

        let paths = staged.activate(&mut ok_gate).expect("activate");

        assert_eq!(paths.hook_path, state_hook);
        assert!(paths.cleanup_incomplete.is_empty());
        let current = with_config_dirs(&dir, &dir, || resolved.handler.inspect(&agent, Ok(&dir)));
        assert_eq!(current.state, IntegrationInstallState::Current);
        assert_eq!(
            current.installed_version,
            Some(EXPECTED_INTEGRATION_VERSION)
        );
        let model = fs::read_to_string(dir.join(if agent == RuntimeRef::claude() {
            "settings.json"
        } else {
            "config.toml"
        }))
        .expect("read provider file");
        assert!(
            model.contains("user-model"),
            "user configuration is preserved"
        );
    }
}

#[test]
fn install_dispatches_through_the_package_runtime_definition() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let claude = scoped_dir("dispatch-claude");
    let codex = scoped_dir("dispatch-codex");
    let host = pi_shaped_integration_host(Path::new("/bin/sh"), CLAUDE_ID, "identity-subagent-v1");
    let pi = RuntimeRef::from_wire("pi");

    let installed = with_config_dirs(&claude, &codex, || install_for(&host, Some(&pi)))
        .expect("install through the package runtime's handler");

    assert_eq!(installed.installed.len(), 1);
    assert_eq!(installed.installed[0].agent, pi);
    assert!(
        Path::new(&installed.installed[0].hook_path).starts_with(&claude),
        "{:?}",
        installed.installed[0]
    );
    assert!(claude.join("hooks").join(STATE_HOOK_INSTALL_NAME).is_file());
    assert!(
        !codex.join(STATE_HOOK_INSTALL_NAME).exists(),
        "the other handler's asset set is not touched"
    );

    let status = with_config_dirs(&claude, &codex, || {
        status_for(
            &host,
            IntegrationStatusParams {
                agent: Some(pi.clone()),
                ..Default::default()
            },
        )
    })
    .expect("status through the package runtime's handler");
    assert_eq!(status.agents.len(), 1);
    assert_eq!(status.agents[0].agent, pi);
    assert_eq!(status.agents[0].state, IntegrationInstallState::Current);

    let removed = with_config_dirs(&claude, &codex, || uninstall_for(&host, &pi))
        .expect("uninstall through the package runtime's handler");
    assert_eq!(removed.uninstalled[0].agent, pi);
    assert_eq!(
        removed.uninstalled[0].state,
        IntegrationUninstallState::Removed
    );
    assert!(!claude.join("hooks").join(STATE_HOOK_INSTALL_NAME).exists());
}

/// A config home that two runtimes reach through one handler (the official
/// `claude` and a package runtime naming the Claude handler with the same
/// config home) belongs to neither: a request without `--agent` reports it
/// unresolved for both and installs nothing into it, while the home only one
/// runtime reaches is acted on once.
#[test]
fn a_home_several_runtimes_reach_through_one_handler_is_attributed_to_none() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let claude = scoped_dir("dispatch-shared-claude");
    let codex = scoped_dir("dispatch-shared-codex");
    let host = pi_shaped_integration_host(Path::new("/bin/sh"), CLAUDE_ID, "identity-subagent-v1");

    let targets: Vec<(String, String, Option<String>)> = with_config_dirs(&claude, &codex, || {
        super::config_homes_for_tests(&host).targets(None, &super::HomeSelection::Runtime)
    })
    .expect("targets")
    .iter()
    .map(|target| {
        (
            target.resolved.runtime_id.as_str().to_owned(),
            target.resolved.handler.id().to_owned(),
            target.home.as_ref().err().map(|error| error.code.clone()),
        )
    })
    .collect();
    let ambiguous = Some("integration_home_ambiguous_owner".to_owned());
    assert_eq!(
        targets,
        [
            ("claude".to_owned(), CLAUDE_ID.to_owned(), ambiguous.clone()),
            ("codex".to_owned(), CODEX_ID.to_owned(), None),
            ("pi".to_owned(), CLAUDE_ID.to_owned(), ambiguous),
        ]
    );

    let refused = with_config_dirs(&claude, &codex, || install_for(&host, None))
        .expect_err("the shared home has no owner");
    assert_eq!(refused.code, "integration_home_ambiguous_owner");
    assert!(refused.msg.contains("`claude`, `pi`"), "{refused:?}");
    assert!(content_snapshot(&claude).is_empty(), "nothing is installed");
}

/// A `subdir` home sits below the variable's value, so a runtime declaring one
/// does not share the home of another runtime that uses the variable bare,
/// while two runtimes declaring the same variable and subdir do share it.
#[test]
fn homes_below_one_variable_are_told_apart_by_their_resolved_path() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let claude = scoped_dir("subdir-claude");
    let codex = scoped_dir("subdir-codex");
    let home_table = |subdir: &str| {
        format!(
            "\n[config_home]\nenv = \"CLAUDE_CONFIG_DIR\"\nsubdir = \"{subdir}\"\ndefault = \".claude/{subdir}\"\n"
        )
    };
    let targets = |homes: &[(&str, &str)]| -> Vec<(String, Result<PathBuf, String>)> {
        let host = pi_shaped_integration_host_with_homes(
            Path::new("/bin/sh"),
            CLAUDE_ID,
            "identity-subagent-v1",
            "\"config-home-subdir-v1\"",
            homes,
        );
        with_config_dirs(&claude, &codex, || {
            super::config_homes_for_tests(&host).targets(None, &super::HomeSelection::Runtime)
        })
        .expect("targets")
        .iter()
        .filter(|target| target.resolved.handler.id() == CLAUDE_ID)
        .map(|target| {
            (
                target.resolved.runtime_id.as_str().to_owned(),
                target
                    .dir()
                    .map(Path::to_path_buf)
                    .map_err(|error| error.code),
            )
        })
        .collect()
    };

    let apart = targets(&[("pi", &home_table("muse")), ("pi2", &home_table("other"))]);
    assert_eq!(
        apart,
        [
            ("claude".to_owned(), Ok(claude.clone())),
            ("pi".to_owned(), Ok(claude.join("muse"))),
            ("pi2".to_owned(), Ok(claude.join("other"))),
        ]
    );

    // Unset, the variable gives way to `default`, which is the complete path;
    // a relative value is an error and never falls back to `default`.
    let one = pi_shaped_integration_host_with_homes(
        Path::new("/bin/sh"),
        CLAUDE_ID,
        "identity-subagent-v1",
        "\"config-home-subdir-v1\"",
        &[("pi", &home_table("muse"))],
    );
    let pi_dir = |claude_dir: Option<&Path>, home: &Path| {
        super::tests::with_status_env(claude_dir, Some(&codex), Some(home), || {
            super::config_homes_for_tests(&one).targets(None, &super::HomeSelection::Runtime)
        })
        .expect("targets")
        .iter()
        .find(|target| target.resolved.runtime_id.as_str() == "pi")
        .map(|target| {
            target
                .dir()
                .map(Path::to_path_buf)
                .map_err(|error| error.code)
        })
        .expect("the pi target")
    };
    assert_eq!(
        pi_dir(None, &claude),
        Ok(claude.join(".claude/muse")),
        "unset: default below HOME"
    );
    assert_eq!(
        pi_dir(Some(Path::new("relative")), &claude),
        Err("agent_config_dir_invalid".to_owned())
    );

    let ambiguous = Err("integration_home_ambiguous_owner".to_owned());
    let shared = targets(&[("pi", &home_table("muse")), ("pi2", &home_table("muse"))]);
    assert_eq!(
        shared,
        [
            ("claude".to_owned(), Ok(claude.clone())),
            ("pi".to_owned(), ambiguous.clone()),
            ("pi2".to_owned(), ambiguous),
        ]
    );
}

/// The home a launch gives an agent that declares a `subdir`: below the
/// variable's value, below a host profile's override of it, the complete
/// `default` while the variable is unset or empty, and an error, never the
/// default, for a value that is not an absolute path.
#[test]
fn a_subdir_home_follows_the_variable_the_profile_and_the_default() {
    use std::ffi::OsString;

    use pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST;

    use crate::agent::host::ConfigHome;
    use crate::runtime::environment::{base_environment, EnvironmentSource};

    const VARIABLE: &str = "XDG_CONFIG_HOME";
    let declared = ConfigHome::new(VARIABLE, ".config/muse")
        .and_then(|home| home.with_subdir("muse"))
        .expect("valid declaration");
    let resolve = |variable: Option<&str>, profile: &[(&str, &str)]| {
        let mut variables = vec![(OsString::from("HOME"), OsString::from("/home/u"))];
        variables.extend(variable.map(|value| (OsString::from(VARIABLE), OsString::from(value))));
        let allowlist: Vec<&str> = DEFAULT_ENVIRONMENT_ALLOWLIST
            .iter()
            .copied()
            .chain([VARIABLE])
            .collect();
        let base = base_environment(&allowlist, &EnvironmentSource::fixed(variables))
            .expect("a base environment");
        let profile: Vec<(String, String)> = profile
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        super::Home::resolve(&declared, &base, &profile)
            .map(|home| home.dir)
            .map_err(|error| error.code)
    };

    assert_eq!(resolve(Some("/x"), &[]), Ok(PathBuf::from("/x/muse")));
    assert_eq!(
        resolve(Some("/x"), &[(VARIABLE, "/p")]),
        Ok(PathBuf::from("/p/muse"))
    );
    assert_eq!(
        resolve(None, &[]),
        Ok(PathBuf::from("/home/u/.config/muse"))
    );
    assert_eq!(
        resolve(Some(""), &[]),
        Ok(PathBuf::from("/home/u/.config/muse"))
    );
    assert_eq!(
        resolve(Some("relative"), &[]),
        Err("agent_config_dir_invalid".to_owned())
    );
}

#[test]
fn a_package_runtime_naming_the_cli_run_handler_is_not_installable() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let dir = scoped_dir("dispatch-cli-run");
    let host = pi_shaped_integration_host(Path::new("/bin/sh"), "hermes-hook-v1", "identity-v1");
    let pi = RuntimeRef::from_wire("pi");

    let install = with_config_dirs(&dir, &dir, || install_for(&host, Some(&pi)))
        .expect_err("the CLI runs this lifecycle");
    assert_eq!(install.code, "agent_not_installable");
    let status = with_config_dirs(&dir, &dir, || {
        status_for(
            &host,
            IntegrationStatusParams {
                agent: Some(pi.clone()),
                ..Default::default()
            },
        )
    })
    .expect_err("the CLI runs this lifecycle");
    assert_eq!(status.code, "agent_not_installable");
    assert_eq!(
        uninstall_for(&host, &pi).expect_err("CLI-run").code,
        "agent_not_installable"
    );
    assert!(content_snapshot(&dir).is_empty());
}

#[test]
fn a_package_runtime_is_named_in_every_recovery_command_of_status_and_doctor() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let dir = scoped_dir("recovery-command");
    let codex = scoped_dir("recovery-command-codex");
    let host = pi_shaped_integration_host(Path::new("/bin/sh"), CLAUDE_ID, "identity-subagent-v1");
    let pi = RuntimeRef::from_wire("pi");
    install_claude(&dir).expect("install");
    let hooks = dir.join("hooks");
    fs::set_permissions(&hooks, fs::Permissions::from_mode(0o777)).expect("loosen hooks");

    let status = with_config_dirs(&dir, &codex, || {
        status_for(
            &host,
            IntegrationStatusParams {
                agent: Some(pi.clone()),
                ..Default::default()
            },
        )
    })
    .expect("status");
    let warnings = status.agents[0].warnings.join("\n");
    assert!(warnings.contains("--agent pi"), "{warnings}");
    assert!(!warnings.contains("--agent claude"), "{warnings}");

    let doctor = with_config_dirs(&dir, &codex, || {
        doctor_for_with(
            &host,
            IntegrationDoctorParams {
                agent: Some(pi.clone()),
                ..Default::default()
            },
            &[],
            &[],
        )
    })
    .expect("doctor");
    let remediations: Vec<&str> = doctor.agents[0]
        .findings
        .iter()
        .filter_map(|finding| finding.remediation.as_deref())
        .collect();
    assert!(!remediations.is_empty());
    for remediation in remediations {
        assert!(!remediation.contains("--agent claude"), "{remediation}");
    }
    assert!(
        doctor.agents[0].findings.iter().any(|finding| finding
            .remediation
            .as_deref()
            .is_some_and(|text| text.contains("pohunek integration install --agent pi"))),
        "{:?}",
        doctor.agents[0].findings
    );

    fs::set_permissions(&hooks, fs::Permissions::from_mode(0o700)).expect("restore hooks");
    fs::remove_dir_all(&hooks).expect("remove hooks");
    fs::remove_file(dir.join("settings.json")).expect("remove settings");
    let absent = with_config_dirs(&dir, &codex, || {
        doctor_for_with(
            &host,
            IntegrationDoctorParams {
                agent: Some(pi.clone()),
                ..Default::default()
            },
            &[],
            &[],
        )
    })
    .expect("doctor");
    assert!(
        absent.agents[0].findings.iter().any(|finding| finding
            .remediation
            .as_deref()
            .is_some_and(|text| text.contains("--agent pi"))),
        "{:?}",
        absent.agents[0].findings
    );
}

/// Runtime ids made of the words `classify_warning` and the not-installed
/// filter look for in free text.
const KEYWORD_RUNTIME_IDS: [&str; 9] = [
    "acme-trust",
    "version-pkg",
    "owned",
    "trust",
    "version",
    "registration",
    "permissions",
    "managed-pkg",
    "missing",
];

/// Doctor findings of `agent` as `(code, summary, remediation)` with the
/// addressed runtime id replaced by a fixed word.
fn doctor_findings(
    host: &RuntimeHost,
    agent: &RuntimeRef,
    dir: &Path,
    other: &Path,
) -> Vec<(String, String, Option<String>)> {
    let result = with_config_dirs(dir, other, || {
        doctor_for_with(
            host,
            IntegrationDoctorParams {
                agent: Some(agent.clone()),
                ..Default::default()
            },
            &[],
            &[],
        )
    })
    .expect("doctor");
    let flag = format!("--agent {}", agent.as_wire());
    result.agents[0]
        .findings
        .iter()
        .map(|finding| {
            (
                serde_json::to_string(&finding.code).expect("code"),
                finding
                    .summary
                    .strip_prefix(&format!("{} ", agent.as_wire()))
                    .map_or_else(
                        || finding.summary.replace(&flag, "--agent ID"),
                        |rest| format!("ID {}", rest.replace(&flag, "--agent ID")),
                    ),
                finding
                    .remediation
                    .as_deref()
                    .map(|text| text.replace(&flag, "--agent ID")),
            )
        })
        .collect()
}

/// Number of states [`break_claude_state`] can put the directory in.
const BROKEN_STATES: usize = 8;

/// Puts the claude config directory `dir` in one of the states whose warnings
/// the doctor classifies by their wording.
fn break_claude_state(dir: &Path, state: usize) {
    let hooks = dir.join("hooks");
    match state {
        0 => fs::set_permissions(&hooks, fs::Permissions::from_mode(0o777)).expect("loosen"),
        1 => fs::write(hooks.join(STATE_HOOK_INSTALL_NAME), "tampered\n").expect("tamper"),
        2 => fs::remove_file(dir.join("settings.json")).expect("remove settings"),
        3 => fs::remove_file(hooks.join(NOTIFY_HOOK_INSTALL_NAME)).expect("remove hook"),
        4 => fs::write(dir.join("settings.json"), "[1]").expect("break settings"),
        5 => fs::set_permissions(
            hooks.join(STATE_HOOK_INSTALL_NAME),
            fs::Permissions::from_mode(0o666),
        )
        .expect("make the hook writable"),
        6 => fs::set_permissions(
            hooks.join(STATE_HOOK_INSTALL_NAME),
            fs::Permissions::from_mode(0o700),
        )
        .expect("drift the hook mode"),
        _ => fs::write(dir.join("settings.json"), "{}").expect("drop the registrations"),
    }
}

#[test]
fn a_runtime_id_never_changes_how_the_doctor_classifies_a_warning() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let other = scoped_dir("classify-other");
    for state in 0..BROKEN_STATES {
        let dir = scoped_dir("classify");
        install_claude(&dir).expect("install");
        break_claude_state(&dir, state);
        let expected = doctor_findings(&official_host(), &RuntimeRef::claude(), &dir, &other);
        assert!(!expected.is_empty(), "state {state} produces findings");

        for id in KEYWORD_RUNTIME_IDS {
            let host = pi_shaped_integration_host_as(
                Path::new("/bin/sh"),
                id,
                CLAUDE_ID,
                "identity-subagent-v1",
            );
            let agent = RuntimeRef::from_wire(id);
            assert_eq!(
                doctor_findings(&host, &agent, &dir, &other),
                expected,
                "state {state} runtime {id}"
            );
        }
    }
}

#[test]
fn a_runtime_id_never_changes_the_not_installed_filter() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let other = scoped_dir("not-installed-other");
    let dir = scoped_dir("not-installed");
    let expected = doctor_findings(&official_host(), &RuntimeRef::claude(), &dir, &other);
    for id in KEYWORD_RUNTIME_IDS {
        let host = pi_shaped_integration_host_as(
            Path::new("/bin/sh"),
            id,
            CLAUDE_ID,
            "identity-subagent-v1",
        );
        assert_eq!(
            doctor_findings(&host, &RuntimeRef::from_wire(id), &dir, &other),
            expected,
            "runtime {id}"
        );
    }
}

#[test]
fn the_codex_scripts_are_the_templates_rendered_from_the_descriptor_values() {
    use super::reporter::ReporterIdentity;
    use super::{
        CODEX_HOOK_ASSET, CODEX_HOOK_TEMPLATE, CODEX_NOTIFY_HOOK_ASSET, CODEX_NOTIFY_HOOK_TEMPLATE,
    };
    let resolved = official(&RuntimeRef::codex());
    let identity = ReporterIdentity::new(resolved.runtime_id.as_str(), &resolved.display_name)
        .expect("the descriptor values are a valid identity");

    assert_eq!(
        identity.render(CODEX_HOOK_TEMPLATE).expect("render"),
        *CODEX_HOOK_ASSET
    );
    assert_eq!(
        identity.render(CODEX_NOTIFY_HOOK_TEMPLATE).expect("render"),
        *CODEX_NOTIFY_HOOK_ASSET
    );
}

#[test]
fn installed_codex_scripts_are_the_rendered_bytes_and_an_unrendered_template_is_drift() {
    use super::{CODEX_HOOK_ASSET, CODEX_HOOK_TEMPLATE};

    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let dir = scoped_dir("rendered-asset-drift");
    let state_hook = install_current(&RuntimeRef::codex(), &dir);
    assert_eq!(
        fs::read_to_string(&state_hook).expect("read installed state hook"),
        *CODEX_HOOK_ASSET
    );
    assert_ne!(*CODEX_HOOK_ASSET, CODEX_HOOK_TEMPLATE);

    fs::write(&state_hook, CODEX_HOOK_TEMPLATE).expect("install the raw template");
    let drifted = with_config_dirs(&dir, &dir, || {
        status_for(
            &official_host(),
            IntegrationStatusParams {
                agent: Some(RuntimeRef::codex()),
                ..Default::default()
            },
        )
    })
    .expect("status")
    .agents
    .pop()
    .expect("Codex report");
    assert_eq!(drifted.state, IntegrationInstallState::Outdated);

    install_current(&RuntimeRef::codex(), &dir);
    assert_eq!(
        fs::read_to_string(&state_hook).expect("read repaired state hook"),
        *CODEX_HOOK_ASSET
    );
}
