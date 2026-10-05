//! Tests of the integration handler registry, staging, compatibility,
//! activation rollback, update, and runtime-definition dispatch.

// Rust guideline compliant 2026-10-05

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use pohunek_worker_protocol::{hook_schema, hook_schemas, is_known_hook_handler, HookAction};
use protocol::{
    ErrorClass, IntegrationDoctorParams, IntegrationInstallState, IntegrationStatusParams,
    IntegrationUninstallState, ProtocolError, RuntimeRef, EXPECTED_INTEGRATION_VERSION,
};

use super::commit::StepGate;
use super::doctor::doctor_for_with;
use super::handler::{
    handler, handlers, managed, resolve, update, Handler, Resolved, UPDATE_INCOMPATIBLE_CODE,
};
use super::tests::{scoped_dir, tree_snapshot, with_config_dirs};
use super::{
    install_claude, install_codex, install_for, status_for, uninstall_for,
    INSTALL_IN_PROGRESS_CODE, INSTALL_LOCK_NAME, INTEGRATION_VERSION_PREFIX,
    NOTIFY_HOOK_INSTALL_NAME, STATE_HOOK_INSTALL_NAME,
};
use crate::agent::host::fixture::{builtin_host, pi_shaped_integration_host};

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

fn builtin(agent: &RuntimeRef) -> Resolved {
    resolve(&builtin_host(), agent).expect("a daemon-run handler")
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
    assert_eq!(ids, [CODEX_ID, CLAUDE_ID, "hermes-hook-v1"]);
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
fn the_daemon_runs_codex_and_claude_and_the_cli_runs_hermes() {
    for id in [CODEX_ID, CLAUDE_ID] {
        assert!(matches!(handler(id), Some(Handler::Daemon(_))), "{id}");
    }
    assert!(matches!(
        handler("hermes-hook-v1"),
        Some(Handler::CliRun(_))
    ));
}

#[test]
fn builtin_runtimes_resolve_through_their_definitions() {
    let host = builtin_host();
    for (agent, id) in [
        (RuntimeRef::claude(), CLAUDE_ID),
        (RuntimeRef::codex(), CODEX_ID),
    ] {
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
            let resolved = builtin(&agent);

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
        let resolved = builtin(&agent);

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
        let status = with_config_dirs(&dir, &dir, || resolved.handler.inspect(&agent));
        assert_eq!(status.state, IntegrationInstallState::Current);
    }
}

#[test]
fn an_active_set_newer_than_the_update_is_replaced_by_it() {
    for agent in agents() {
        let dir = scoped_dir("compat-newer-active");
        write_user_config(&agent, &dir);
        let state_hook = install_current(&agent, &dir);
        set_script_version(&state_hook, EXPECTED_INTEGRATION_VERSION + 1);
        let resolved = builtin(&agent);

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

        let status = with_config_dirs(&dir, &dir, || resolved.handler.inspect(&agent));
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
                let resolved = builtin(&agent);
                let mut gate = |index: usize, _name: &str| {
                    if index == fail_at {
                        Err(injected())
                    } else {
                        Ok(())
                    }
                };

                let outcome = update(&resolved, &dir, &mut gate);

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
fn updating_from_the_previous_asset_set_activates_atomically() {
    for agent in agents() {
        let dir = scoped_dir("update-previous");
        write_user_config(&agent, &dir);
        let state_hook = install_current(&agent, &dir);
        set_script_version(&state_hook, EXPECTED_INTEGRATION_VERSION - 1);
        let previous = content_snapshot(&dir);
        let resolved = builtin(&agent);
        let outdated = with_config_dirs(&dir, &dir, || resolved.handler.inspect(&agent));
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
        let current = with_config_dirs(&dir, &dir, || resolved.handler.inspect(&agent));
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

#[test]
fn a_handler_is_reached_once_when_several_runtimes_name_it() {
    let claude = scoped_dir("dispatch-once-claude");
    let codex = scoped_dir("dispatch-once-codex");
    let host = pi_shaped_integration_host(Path::new("/bin/sh"), CLAUDE_ID, "identity-subagent-v1");

    let ids: Vec<String> = managed(&host)
        .iter()
        .map(|resolved| resolved.handler.id().to_owned())
        .collect();
    assert_eq!(ids, [CLAUDE_ID, CODEX_ID]);

    let installed = with_config_dirs(&claude, &codex, || install_for(&host, None))
        .expect("install every present handler once");
    let agents: Vec<&RuntimeRef> = installed
        .installed
        .iter()
        .map(|report| &report.agent)
        .collect();
    assert_eq!(agents, [&RuntimeRef::claude(), &RuntimeRef::codex()]);
}

#[test]
fn a_package_runtime_naming_the_cli_run_handler_is_not_installable() {
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
fn the_activation_gate_runs_before_every_committed_file() {
    let dir = scoped_dir("gate-order");
    let resolved = builtin(&RuntimeRef::claude());
    let staged = resolved.handler.stage(&dir).expect("stage");
    let mut seen = Vec::new();
    let mut gate = |index: usize, name: &str| {
        seen.push((index, name.to_owned()));
        Ok(())
    };
    let gate_ref: StepGate<'_> = &mut gate;
    staged.activate(gate_ref).expect("activate");
    assert_eq!(
        seen.last().map(|(_index, name)| name.as_str()),
        Some("settings.json"),
        "registration is committed last: {seen:?}"
    );
}

#[test]
fn a_package_runtime_is_named_in_every_recovery_command_of_status_and_doctor() {
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
