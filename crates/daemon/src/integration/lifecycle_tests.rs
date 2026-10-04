//! Lifecycle, transaction, socket-reach, and secrecy tests for the Claude and
//! Codex integrations.
//!
//! Every test runs natively on Linux and macOS: fixtures live under the
//! symlink-free, short test roots, and hooks reach real Unix sockets.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::{symlink, FileTypeExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;

use pohunek_paths::{BasePaths, PathEnv, PathError, Platform, DARWIN_SOCKET_PATH_MAX_BYTES};
use protocol::{
    ErrorClass, IntegrationInstallState, IntegrationRecovery, IntegrationStatusParams,
    ProtocolError, RuntimeId, RuntimeRef,
};
use serde_json::{json, Value};

use super::commit::{DESTINATION_COLLISION_CODE, RECOVERY_REQUIRED_CODE};
use super::removal_tests::RaceHook;
use super::tests::{
    capture_worker_hook, explicit_status, inherited_path, read_json, run_state_asset_at,
    scoped_dir, tree_snapshot, with_config_dirs, with_config_dirs_and,
};
use super::{
    hook_command, install_claude, install_claude_gated, install_codex, install_codex_gated,
    TrustedDir, CLAUDE_HOOK_ASSET, CLAUDE_NOTIFY_HOOK_ASSET, CODEX_HOOK_ASSET,
    CODEX_NOTIFY_HOOK_ASSET, HOOK_ACTION, INSTALL_IN_PROGRESS_CODE, INSTALL_LOCK_NAME,
    NOTIFY_HOOK_INSTALL_NAME, STATE_HOOK_INSTALL_NAME,
};

/// Error code raised by the injected gate failures below.
const INJECTED_CODE: &str = "injected_step_failure";

/// Threads racing one installer lock in the concurrency test.
const CONCURRENT_INSTALLERS: usize = 8;

/// Mask selecting the file-type bits of a Unix mode.
const FILE_TYPE_MASK: u32 = 0o170_000;

/// File-type bits of a directory.
const DIRECTORY_TYPE: u32 = 0o040_000;

/// Requests a state hook sends for one `SessionStart` report.
const SESSION_REPORT_REQUESTS: usize = 2;

/// Owner-private mode of runtime directories the fixtures create.
const RUNTIME_DIR_MODE: u32 = 0o700;

/// Effective uid used to resolve the default macOS runtime directory.
const FIXTURE_UID: u32 = 501;

/// Worker session ID of the longest accepted grammar (ULID form).
const LONGEST_WORKER_SESSION_ID: &str = "s-01ARZ3NDEKTSV4RRFFQ69G5FAV";

/// Characters of the staged worker socket name (`.s` plus its hex suffix).
const STAGED_WORKER_SOCKET_NAME_BYTES: usize = 18;

/// Secret-looking values planted in provider files and payloads.
const SENTINELS: [&str; 4] = [
    "sk-sentinel-config-0001",
    "sk-sentinel-toml-0002",
    "sk-sentinel-env-0003",
    "sk-sentinel-payload-0004",
];

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

fn user_claude_settings() -> Value {
    json!({
        "model": "user-model",
        "hooks": {
            "SessionStart": [
                { "matcher": "*", "hooks": [{ "type": "command", "command": "echo user-start" }] }
            ]
        }
    })
}

fn write_json(path: &Path, value: &Value) {
    fs::write(
        path,
        serde_json::to_string_pretty(value).expect("serialize"),
    )
    .expect("write json");
}

fn managed_commands(settings: &Value) -> Vec<String> {
    let mut commands = Vec::new();
    if let Some(events) = settings["hooks"].as_object() {
        for entries in events.values().filter_map(Value::as_array) {
            for hook in entries
                .iter()
                .filter_map(|entry| entry["hooks"].as_array())
                .flatten()
            {
                if let Some(command) = hook["command"].as_str() {
                    commands.push(command.to_owned());
                }
            }
        }
    }
    commands.retain(|command| command.contains("pohunek-agent-"));
    commands
}

fn status_of(dir: &Path, agent: RuntimeRef) -> protocol::IntegrationAgentStatus {
    explicit_status(dir, agent)
}

#[test]
fn installer_lock_rejects_a_second_installer_without_touching_the_tree() {
    for agent in [RuntimeRef::claude(), RuntimeRef::codex()] {
        let dir = scoped_dir("lock-contention");
        let install = |dir: &Path| match agent.as_wire() {
            RuntimeId::CLAUDE => install_claude(dir).map(|_paths| ()),
            _ => install_codex(dir).map(|_paths| ()),
        };
        install(&dir).expect("first install");
        let before = tree_snapshot(&dir);

        let holder = TrustedDir::open(&dir, "test root").expect("open trusted dir");
        let lock = holder.lock_installer().expect("hold installer lock");
        let error = install(&dir).expect_err("second installer must not proceed");
        assert_eq!(error.code, INSTALL_IN_PROGRESS_CODE, "{agent:?}");
        assert_eq!(tree_snapshot(&dir), before, "{agent:?} tree changed");

        drop(lock);
        install(&dir).expect("install after the lock is released");
        assert_eq!(tree_snapshot(&dir), before, "{agent:?} reinstall drifted");
    }
}

#[test]
fn concurrent_installers_neither_lose_user_hooks_nor_duplicate_registrations() {
    let dir = scoped_dir("lock-race");
    write_json(&dir.join("settings.json"), &user_claude_settings());
    let barrier = Arc::new(Barrier::new(CONCURRENT_INSTALLERS));
    let workers: Vec<_> = (0..CONCURRENT_INSTALLERS)
        .map(|_| {
            let dir = dir.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                install_claude(&dir).map(|_paths| ())
            })
        })
        .collect();
    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().expect("installer thread"))
        .collect();

    assert!(
        results.iter().any(Result::is_ok),
        "one installer must win the lock"
    );
    for result in &results {
        if let Err(error) = result {
            assert_eq!(error.code, INSTALL_IN_PROGRESS_CODE);
        }
    }
    install_claude(&dir).expect("settle install");
    let settings = read_json(&dir.join("settings.json"));
    let commands = managed_commands(&settings);
    let mut unique = commands.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), commands.len(), "duplicate registrations");
    assert_eq!(settings["model"], json!("user-model"));
    assert!(
        settings["hooks"]["SessionStart"]
            .to_string()
            .contains("echo user-start"),
        "user hook lost: {settings}"
    );
    let report = status_of(&dir, RuntimeRef::claude());
    assert_eq!(report.state, IntegrationInstallState::Current);
}

/// Step names committed by a successful install into a fresh directory.
fn committed_step_names(agent: &RuntimeRef) -> Vec<String> {
    let dir = scoped_dir("step-names");
    let mut seen = Vec::new();
    let mut gate = |_index: usize, name: &str| {
        seen.push(name.to_owned());
        Ok(())
    };
    match agent.as_wire() {
        RuntimeId::CLAUDE => install_claude_gated(&dir, &mut gate).map(|_paths| ()),
        _ => install_codex_gated(&dir, &mut gate).map(|_paths| ()),
    }
    .expect("install for step names");
    seen
}

#[test]
fn commit_steps_run_scripts_before_registration() {
    assert_eq!(
        committed_step_names(&RuntimeRef::claude()),
        [
            STATE_HOOK_INSTALL_NAME,
            NOTIFY_HOOK_INSTALL_NAME,
            "settings.json"
        ]
    );
    assert_eq!(
        committed_step_names(&RuntimeRef::codex()),
        [
            STATE_HOOK_INSTALL_NAME,
            NOTIFY_HOOK_INSTALL_NAME,
            "hooks.json",
            "config.toml"
        ]
    );
}

/// Installs, then rewrites the tree into an older-looking drifted state so the
/// next install replaces existing content instead of creating it.
fn prepare_existing(agent: &RuntimeRef, dir: &Path) {
    if *agent == RuntimeRef::claude() {
        write_json(&dir.join("settings.json"), &user_claude_settings());
        install_claude(dir).expect("seed install");
        fs::write(dir.join("hooks").join(STATE_HOOK_INSTALL_NAME), "# old\n")
            .expect("age state hook");
    } else {
        fs::write(dir.join("config.toml"), "model = \"user-model\"\n").expect("seed config");
        install_codex(dir).expect("seed install");
        fs::write(dir.join(STATE_HOOK_INSTALL_NAME), "# old\n").expect("age state hook");
        fs::write(dir.join("config.toml"), "model = \"other-user-model\"\n").expect("age config");
    }
}

#[test]
fn injected_failure_at_every_commit_step_restores_the_exact_prior_tree() {
    for agent in [RuntimeRef::claude(), RuntimeRef::codex()] {
        let steps = committed_step_names(&agent).len();
        for existing in [false, true] {
            for fail_at in 0..steps {
                let dir = scoped_dir("rollback");
                if existing {
                    prepare_existing(&agent, &dir);
                }
                let before = content_snapshot(&dir);
                let mut gate = |index: usize, _name: &str| {
                    if index == fail_at {
                        Err(injected())
                    } else {
                        Ok(())
                    }
                };

                let error = match agent.as_wire() {
                    RuntimeId::CLAUDE => install_claude_gated(&dir, &mut gate).map(|_paths| ()),
                    _ => install_codex_gated(&dir, &mut gate).map(|_paths| ()),
                }
                .expect_err("gate failure aborts the install");

                assert_eq!(error.code, INJECTED_CODE);
                assert_eq!(
                    content_snapshot(&dir),
                    before,
                    "{agent:?} existing={existing} fail_at={fail_at} left residue"
                );
            }
        }
    }
}

#[test]
fn a_provider_file_edited_during_the_install_is_a_collision_and_stays_intact() {
    let dir = scoped_dir("collision");
    let concurrent_edit = json!({ "edited": "by someone else" });
    let mut gate = |index: usize, name: &str| {
        if name == "settings.json" {
            assert_eq!(index, 2);
            write_json(&dir.join("settings.json"), &concurrent_edit);
        }
        Ok(())
    };

    let error = install_claude_gated(&dir, &mut gate).expect_err("collision must abort");

    assert_eq!(error.code, DESTINATION_COLLISION_CODE);
    assert_eq!(read_json(&dir.join("settings.json")), concurrent_edit);
    assert!(!dir.join("hooks").join(STATE_HOOK_INSTALL_NAME).exists());
    assert!(!dir.join("hooks").join(NOTIFY_HOOK_INSTALL_NAME).exists());
}

#[test]
fn a_directory_at_a_managed_hook_path_is_preserved_and_reported_untrusted() {
    let dir = scoped_dir("dir-at-hook");
    let notify = dir.join("hooks").join(NOTIFY_HOOK_INSTALL_NAME);
    let mut gate = |_index: usize, name: &str| {
        if name == NOTIFY_HOOK_INSTALL_NAME {
            fs::create_dir(&notify).expect("place foreign directory");
        }
        Ok(())
    };

    let error = install_claude_gated(&dir, &mut gate).expect_err("foreign directory aborts");

    assert_eq!(error.code, "integration_path_untrusted");
    assert!(notify.is_dir(), "foreign directory must be preserved");
    assert!(!dir.join("hooks").join(STATE_HOOK_INSTALL_NAME).exists());
    assert!(!dir.join("settings.json").exists());
}

#[test]
fn a_fifo_at_a_managed_hook_path_is_rejected_without_blocking_or_mutation() {
    let dir = scoped_dir("fifo-at-hook");
    let hook = dir.join(STATE_HOOK_INSTALL_NAME);
    let output = pohunek_test_support::process_env::command("mkfifo")
        .arg(&hook)
        .output()
        .expect("run mkfifo");
    assert!(output.status.success(), "mkfifo failed");
    let before = content_snapshot(&dir);

    let error = install_codex(&dir).expect_err("FIFO at a managed path aborts");

    assert_eq!(error.code, "integration_path_untrusted");
    assert_eq!(content_snapshot(&dir), before);
    assert!(fs::symlink_metadata(&hook)
        .expect("FIFO metadata")
        .file_type()
        .is_fifo());
}

#[test]
fn a_written_file_replaced_before_the_rollback_is_kept_and_reported_as_a_collision() {
    for existing in [false, true] {
        let dir = scoped_dir("recovery");
        if existing {
            prepare_existing(&RuntimeRef::codex(), &dir);
        }
        let state = dir.join(STATE_HOOK_INSTALL_NAME);
        let mut gate = |_index: usize, name: &str| {
            if name == "config.toml" {
                fs::remove_file(&state).expect("remove committed state hook");
                fs::create_dir(&state).expect("shadow it with a directory");
                return Err(injected());
            }
            Ok(())
        };

        let error = install_codex_gated(&dir, &mut gate).expect_err("failure aborts");

        assert_eq!(
            error.code, DESTINATION_COLLISION_CODE,
            "existing={existing}"
        );
        assert!(
            error.msg.contains(&state.display().to_string()),
            "{}",
            error.msg
        );
        assert!(error.msg.contains(INJECTED_CODE), "{}", error.msg);
        assert!(state.is_dir(), "the shadowing directory must be preserved");
        assert!(
            !dir.join(NOTIFY_HOOK_INSTALL_NAME).exists() || existing,
            "restorable steps are still rolled back"
        );
    }
}

#[test]
fn a_displaced_managed_symlink_is_restored_when_a_later_step_fails() {
    let dir = scoped_dir("symlink-rollback");
    install_codex(&dir).expect("seed install");
    let target = dir.join("sentinel-target");
    fs::write(&target, b"private-target").expect("write target");
    let hook = dir.join(STATE_HOOK_INSTALL_NAME);
    fs::remove_file(&hook).expect("remove hook");
    symlink(&target, &hook).expect("link hook to target");
    let mut gate = |index: usize, _name: &str| {
        if index == 2 {
            Err(injected())
        } else {
            Ok(())
        }
    };

    let error = install_codex_gated(&dir, &mut gate).expect_err("failure aborts");

    assert_eq!(error.code, INJECTED_CODE);
    assert_eq!(fs::read_link(&hook).expect("symlink restored"), target);
    assert_eq!(fs::read(&target).expect("target intact"), b"private-target");
}

#[test]
fn reinstall_and_upgrade_are_idempotent_for_each_agent() {
    for agent in [RuntimeRef::claude(), RuntimeRef::codex()] {
        let dir = scoped_dir("idempotent");
        prepare_existing(&agent, &dir);
        let install = |dir: &Path| match agent.as_wire() {
            RuntimeId::CLAUDE => install_claude(dir).map(|_paths| ()),
            _ => install_codex(dir).map(|_paths| ()),
        };

        install(&dir).expect("upgrade from drifted state");
        let upgraded = tree_snapshot(&dir);
        assert_eq!(
            status_of(&dir, agent.clone()).state,
            IntegrationInstallState::Current,
            "{agent:?}"
        );
        install(&dir).expect("reinstall");

        assert_eq!(tree_snapshot(&dir), upgraded, "{agent:?} reinstall drifted");
    }
}

#[test]
fn installed_assets_match_the_embedded_scripts_with_owner_only_write_modes() {
    let claude = scoped_dir("modes-claude");
    let codex = scoped_dir("modes-codex");
    install_claude(&claude).expect("install Claude");
    install_codex(&codex).expect("install Codex");

    for (path, expected) in [
        (
            claude.join("hooks").join(STATE_HOOK_INSTALL_NAME),
            CLAUDE_HOOK_ASSET,
        ),
        (
            claude.join("hooks").join(NOTIFY_HOOK_INSTALL_NAME),
            CLAUDE_NOTIFY_HOOK_ASSET,
        ),
        (codex.join(STATE_HOOK_INSTALL_NAME), CODEX_HOOK_ASSET),
        (
            codex.join(NOTIFY_HOOK_INSTALL_NAME),
            CODEX_NOTIFY_HOOK_ASSET,
        ),
    ] {
        assert_eq!(fs::read_to_string(&path).expect("read asset"), expected);
        let mode = fs::metadata(&path)
            .expect("asset metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o7777,
            super::MANAGED_HOOK_MODE,
            "{}",
            path.display()
        );
    }
}

#[test]
fn profiles_are_isolated_by_directory_and_exact_command_ownership() {
    let profile_a = scoped_dir("profile-a");
    let profile_b = scoped_dir("profile-b");
    let hook_a = profile_a.join("hooks").join(STATE_HOOK_INSTALL_NAME);
    let foreign_command = hook_command(&hook_a, HOOK_ACTION);
    let mut settings_b = user_claude_settings();
    settings_b["hooks"]["SessionStart"][0]["hooks"]
        .as_array_mut()
        .expect("hooks array")
        .push(json!({ "type": "command", "command": foreign_command }));
    write_json(&profile_b.join("settings.json"), &settings_b);

    install_claude(&profile_a).expect("install profile A");
    let untouched_b = tree_snapshot(&profile_b);
    assert!(
        !profile_b.join("hooks").exists(),
        "installing A must not create B's hooks"
    );
    install_claude(&profile_b).expect("install profile B");

    let commands_b = managed_commands(&read_json(&profile_b.join("settings.json")));
    let hook_b = profile_b.join("hooks");
    assert!(
        commands_b.contains(&foreign_command),
        "A's exact command is not B's to strip"
    );
    assert!(
        commands_b
            .iter()
            .filter(|command| **command != foreign_command)
            .all(|command| command.contains(&hook_b.display().to_string())),
        "B registers only its own hook paths: {commands_b:?}"
    );
    let commands_a = managed_commands(&read_json(&profile_a.join("settings.json")));
    assert!(commands_a
        .iter()
        .all(|command| command.contains(&profile_a.display().to_string())));

    let settled_b = tree_snapshot(&profile_b);
    install_claude(&profile_a).expect("reinstall profile A");
    assert_eq!(tree_snapshot(&profile_b), settled_b);
    assert_ne!(untouched_b, settled_b);
    assert_eq!(
        status_of(&profile_a, RuntimeRef::claude()).state,
        IntegrationInstallState::Current
    );
}

#[test]
fn explicit_agent_home_selects_exactly_the_targeted_directory() {
    let claude = scoped_dir("target-claude");
    let codex = scoped_dir("target-codex");
    let codex_before = tree_snapshot(&codex);

    let result = with_config_dirs(&claude, &codex, || {
        super::install(Some(RuntimeRef::claude()))
    })
    .expect("install Claude only");

    assert_eq!(result.installed.len(), 1);
    assert_eq!(result.installed[0].agent, RuntimeRef::claude());
    assert_eq!(
        tree_snapshot(&codex),
        codex_before,
        "Codex must be untouched"
    );

    let missing_claude = codex.join("no-claude-here");
    let result = with_config_dirs(&missing_claude, &codex, || super::install(None))
        .expect("install every present agent");
    assert_eq!(result.installed.len(), 1);
    assert_eq!(result.installed[0].agent, RuntimeRef::codex());
    assert!(
        !missing_claude.exists(),
        "absent agent dir is never created"
    );

    let absent = scoped_dir("target-absent");
    let error = with_config_dirs(&absent.join("c"), &absent.join("x"), || {
        super::install(None)
    })
    .expect_err("nothing to install");
    assert_eq!(error.code, "agent_config_dir_missing");
    assert!(!absent.join("c").exists() && !absent.join("x").exists());
}

#[test]
fn a_symlinked_config_root_is_rejected_and_its_canonical_path_installs() {
    let root = pohunek_test_support::tempdir().expect("fixture root");
    let real = root.path().join("real");
    fs::create_dir(&real).expect("create real dir");
    fs::set_permissions(&real, fs::Permissions::from_mode(RUNTIME_DIR_MODE))
        .expect("private real dir");
    let alias = root.path().join("alias");
    symlink(&real, &alias).expect("create alias");

    let error = install_codex(&alias).expect_err("symlinked config root is untrusted");

    assert_eq!(error.code, "integration_path_untrusted");
    assert!(
        fs::read_dir(&real).expect("read real dir").next().is_none(),
        "rejection must not mutate the target"
    );
    let canonical = fs::canonicalize(&alias).expect("canonicalize alias");
    install_codex(&canonical).expect("canonical path installs");
    assert_eq!(
        status_of(&canonical, RuntimeRef::codex()).state,
        IntegrationInstallState::Current
    );
}

#[test]
fn a_missing_optional_agent_is_informational_and_a_broken_config_root_is_a_failure() {
    let root = scoped_dir("optional-agent");
    let absent = root.join("absent");
    let not_a_directory = root.join("file");
    fs::write(&not_a_directory, "").expect("write file");

    let optional = status_of(&absent, RuntimeRef::claude());
    assert!(!optional.available);
    assert_eq!(optional.state, IntegrationInstallState::NotInstalled);
    assert_eq!(optional.recovery, IntegrationRecovery::None);
    assert!(optional.warnings.is_empty(), "{:?}", optional.warnings);
    assert!(!absent.exists(), "status never creates the directory");

    let broken = status_of(&not_a_directory, RuntimeRef::codex());
    assert!(!broken.available);
    assert_eq!(broken.state, IntegrationInstallState::Outdated);
    assert_eq!(broken.recovery, IntegrationRecovery::RepairConfiguration);
    assert_eq!(broken.warnings, ["agent config path is not a directory"]);
}

#[test]
fn an_unresolvable_config_root_is_a_failure_not_an_absent_agent() {
    let report = with_config_dirs(
        Path::new("relative/claude"),
        Path::new("/nonexistent"),
        || {
            super::status(IntegrationStatusParams {
                agent: Some(RuntimeRef::claude()),
            })
        },
    )
    .expect("status degrades")
    .agents
    .remove(0);

    assert_eq!(report.state, IntegrationInstallState::Outdated);
    assert_eq!(report.recovery, IntegrationRecovery::RepairConfiguration);
    assert!(!report.warnings.is_empty());
}

fn macos_env(runtime_dir: &Path) -> PathEnv {
    PathEnv {
        xdg_runtime_dir: Some(runtime_dir.as_os_str().to_owned()),
        home: Some("/Users/fixture".into()),
        ..PathEnv::default()
    }
}

/// Pads a runtime directory name so `suffix` below it ends exactly at `total`.
fn padded_runtime_dir(root: &Path, suffix_bytes: usize, total: usize) -> PathBuf {
    let base = root.as_os_str().len() + 1;
    let pad = total
        .checked_sub(base + suffix_bytes)
        .expect("fixture root leaves room for padding");
    root.join("r".repeat(pad))
}

fn assert_hook_delivers(agent: &str, listener: &Path, env_socket: &Path) {
    let input = json!({ "session_id": format!("{agent}-native") });
    let (status, stdout, stderr, requests) = run_state_asset_at(
        agent,
        &["session"],
        &input,
        Some(listener),
        env_socket,
        SESSION_REPORT_REQUESTS,
        None,
    );
    assert!(
        status.success(),
        "{agent} hook exited with {status}: {stderr}"
    );
    assert_eq!(stdout, "");
    assert_eq!(stderr, "");
    assert_eq!(
        requests.len(),
        SESSION_REPORT_REQUESTS,
        "{agent} hook must reach {}",
        env_socket.display()
    );
}

#[test]
fn hooks_reach_a_daemon_socket_resolved_at_the_darwin_length_limit() {
    let root = pohunek_test_support::tempdir().expect("fixture root");
    let runtime = padded_runtime_dir(
        root.path(),
        "/pohunek/daemon.sock".len(),
        DARWIN_SOCKET_PATH_MAX_BYTES,
    );
    let paths = BasePaths::resolve_for(Platform::MacOs, FIXTURE_UID, &macos_env(&runtime))
        .expect("daemon resolves the limit-length socket path");
    assert_eq!(paths.socket.as_os_str().len(), DARWIN_SOCKET_PATH_MAX_BYTES);
    fs::create_dir_all(&paths.runtime_dir).expect("create runtime dir");

    for agent in ["claude", "codex"] {
        let socket = paths.socket.clone();
        assert_hook_delivers(agent, &socket, &socket);
        fs::remove_file(&socket).expect("remove bound socket");
    }

    let too_long = padded_runtime_dir(
        root.path(),
        "/pohunek/daemon.sock".len(),
        DARWIN_SOCKET_PATH_MAX_BYTES + 1,
    );
    let error = BasePaths::resolve_for(Platform::MacOs, FIXTURE_UID, &macos_env(&too_long))
        .expect_err("the daemon never publishes an unconnectable path");
    assert!(
        matches!(error, PathError::SocketPathTooLong { .. }),
        "{error}"
    );
}

#[test]
fn hooks_reach_the_socket_through_a_symlinked_alias_of_the_runtime_dir() {
    let root = pohunek_test_support::tempdir().expect("fixture root");
    let real = root.path().join("real");
    fs::create_dir(&real).expect("create real runtime dir");
    let alias = root.path().join("alias");
    symlink(&real, &alias).expect("create alias");
    let listener = real.join("daemon.sock");
    let via_alias = alias.join("daemon.sock");

    for agent in ["claude", "codex"] {
        assert_hook_delivers(agent, &listener, &via_alias);
        fs::remove_file(&listener).expect("remove bound socket");
    }
}

#[test]
fn the_default_macos_runtime_socket_is_canonical_and_fits_darwin() {
    let env = PathEnv {
        home: Some("/Users/fixture".into()),
        ..PathEnv::default()
    };

    for uid in [0, FIXTURE_UID, u32::MAX] {
        let paths = BasePaths::resolve_for(Platform::MacOs, uid, &env)
            .expect("default macOS runtime resolves");
        assert_eq!(
            paths.socket,
            // hermetic-allowed: #363 the macOS default runtime path under /private/tmp is the subject
            PathBuf::from(format!("/private/tmp/pohunek-{uid}/daemon.sock"))
        );
        assert!(paths.socket.as_os_str().len() <= DARWIN_SOCKET_PATH_MAX_BYTES);
    }
}

#[test]
fn hooks_reach_a_worker_socket_resolved_at_the_darwin_staged_limit() {
    let root = pohunek_test_support::tempdir().expect("fixture root");
    let session_suffix =
        "/workers/".len() + LONGEST_WORKER_SESSION_ID.len() + 1 + STAGED_WORKER_SOCKET_NAME_BYTES;
    let runtime = padded_runtime_dir(
        root.path(),
        "/pohunek".len() + session_suffix,
        DARWIN_SOCKET_PATH_MAX_BYTES,
    );
    let paths = BasePaths::resolve_for(Platform::MacOs, FIXTURE_UID, &macos_env(&runtime))
        .expect("daemon resolves");
    let socket = paths
        .worker_socket(LONGEST_WORKER_SESSION_ID)
        .expect("worker socket fits the staged limit")
        .expect("valid worker session id");
    fs::create_dir_all(socket.parent().expect("worker dir")).expect("create worker dir");
    let path = inherited_path();
    let capture = capture_worker_hook(
        &socket,
        b"{\"ok\":true,\"launch_identity_accepted\":true}\n",
    );

    let mut child = std::process::Command::new("sh")
        .arg(super::tests::state_asset("claude"))
        .arg("session")
        .env_clear()
        .env("PATH", path)
        .env(super::ENV_FLAG, "1")
        .env("POHUNEK_WORKER_SOCKET_PATH", &socket)
        .env("POHUNEK_NATIVE_REFERENCE_KIND", "id")
        .env(super::ENV_SESSION_ID, "session-123")
        .env("POHUNEK_WORKER_INSTANCE_ID", "runtime-123")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn worker hook");
    std::io::Write::write_all(
        &mut child.stdin.take().expect("hook stdin"),
        json!({ "session_id": "claude-native" })
            .to_string()
            .as_bytes(),
    )
    .expect("write hook stdin");
    let output = child.wait_with_output().expect("wait for hook");
    let request = capture.join().expect("worker capture");

    assert!(output.status.success());
    assert_eq!(request["type"], json!("identity_report"));

    let too_long = padded_runtime_dir(
        root.path(),
        "/pohunek".len() + session_suffix,
        DARWIN_SOCKET_PATH_MAX_BYTES + 1,
    );
    let over = BasePaths::resolve_for(Platform::MacOs, FIXTURE_UID, &macos_env(&too_long))
        .expect("daemon socket still fits");
    let error = over
        .worker_socket(LONGEST_WORKER_SESSION_ID)
        .expect_err("worker path over the staged limit is refused before launch");
    assert!(
        matches!(error, PathError::SocketPathTooLong { .. }),
        "{error}"
    );
}

#[test]
fn hook_assets_hold_no_node_or_bun_executable_exceptions() {
    for asset in [
        CLAUDE_HOOK_ASSET,
        CLAUDE_NOTIFY_HOOK_ASSET,
        CODEX_HOOK_ASSET,
        CODEX_NOTIFY_HOOK_ASSET,
    ] {
        for token in asset.split(|character: char| !character.is_ascii_alphanumeric()) {
            assert!(
                !["node", "nodejs", "bun", "deno"].contains(&token.to_ascii_lowercase().as_str()),
                "hook assets must not special-case a JavaScript runtime executable: {token}"
            );
        }
    }
}

fn assert_no_sentinel(label: &str, text: &str) {
    for sentinel in SENTINELS {
        assert!(
            !text.contains(sentinel),
            "{label} leaked {sentinel}: {text}"
        );
    }
}

fn error_text(error: &ProtocolError) -> String {
    format!("{} {} {:?} {error:?}", error.code, error.msg, error.recover)
}

#[test]
fn provider_secrets_never_reach_install_status_or_error_output() {
    let claude = scoped_dir("secret-claude");
    let codex = scoped_dir("secret-codex");
    write_json(
        &claude.join("settings.json"),
        &json!({
            "env": { "ANTHROPIC_API_KEY": SENTINELS[0] },
            "apiKeyHelper": SENTINELS[0],
            "hooks": {
                "PreToolUse": [
                    { "matcher": "*", "hooks": [
                        { "type": "command", "command": format!("echo {}", SENTINELS[0]) }
                    ]}
                ]
            }
        }),
    );
    fs::write(
        codex.join("config.toml"),
        format!("api_key = \"{}\"\n", SENTINELS[1]),
    )
    .expect("write Codex config");

    let mut outputs = Vec::new();
    let provider_keys = [
        ("ANTHROPIC_API_KEY", SENTINELS[2]),
        ("OPENAI_API_KEY", SENTINELS[2]),
    ];
    let (installed, statuses) = with_config_dirs_and(&claude, &codex, &provider_keys, || {
        let installed = super::install(None);
        let statuses = super::status(IntegrationStatusParams { agent: None });
        (installed, statuses)
    });
    outputs.push(format!("{:?}", installed.expect("install")));
    outputs.push(serde_json::to_string(&statuses.expect("status")).expect("status json"));

    for (label, text) in [
        (
            "claude settings survive verbatim",
            read_settings_text(&claude),
        ),
        ("codex config survives verbatim", read_config_text(&codex)),
    ] {
        assert!(
            text.contains(SENTINELS[0]) || text.contains(SENTINELS[1]),
            "{label}: user data must be preserved"
        );
    }

    fs::write(
        codex.join("config.toml"),
        format!("api_key = \"{}\" trailing garbage\n", SENTINELS[1]),
    )
    .expect("corrupt Codex config");
    outputs.push(error_text(
        &install_codex(&codex).expect_err("malformed TOML aborts"),
    ));
    outputs.push(format!("{:?}", status_of(&codex, RuntimeRef::codex())));

    fs::write(
        claude.join("settings.json"),
        format!("{{\"env\": \"{}\" oops", SENTINELS[0]),
    )
    .expect("corrupt Claude settings");
    outputs.push(error_text(
        &install_claude(&claude).expect_err("malformed JSON aborts"),
    ));
    outputs.push(format!("{:?}", status_of(&claude, RuntimeRef::claude())));

    for (index, text) in outputs.iter().enumerate() {
        assert_no_sentinel(&format!("output {index}"), text);
    }
}

fn read_settings_text(dir: &Path) -> String {
    fs::read_to_string(dir.join("settings.json")).expect("read settings")
}

fn read_config_text(dir: &Path) -> String {
    fs::read_to_string(dir.join("config.toml")).expect("read config")
}

#[test]
fn state_hooks_forward_no_provider_payload_beyond_the_reported_identity() {
    let root = pohunek_test_support::tempdir().expect("fixture root");
    let socket = root.path().join("d.sock");
    let input = json!({
        "session_id": "claude-native",
        "prompt": SENTINELS[3],
        "tool_input": { "command": SENTINELS[3] },
        "api_key": SENTINELS[3],
    });

    let (status, stdout, stderr, requests) = run_state_asset_at(
        "claude",
        &["session"],
        &input,
        Some(&socket),
        &socket,
        SESSION_REPORT_REQUESTS,
        None,
    );

    assert!(status.success(), "{stderr}");
    assert_eq!(requests.len(), SESSION_REPORT_REQUESTS);
    assert_no_sentinel("stdout", &stdout);
    assert_no_sentinel("stderr", &stderr);
    assert_no_sentinel("requests", &Value::Array(requests).to_string());
}

#[test]
fn a_failed_install_removes_the_hooks_directory_it_created_and_keeps_an_existing_one() {
    for fail_at in 0..3 {
        let fresh = scoped_dir("created-hooks");
        let mut gate = |index: usize, _name: &str| {
            if index == fail_at {
                Err(injected())
            } else {
                Ok(())
            }
        };
        install_claude_gated(&fresh, &mut gate).expect_err("gate failure aborts");
        assert!(
            !fresh.join("hooks").exists(),
            "fail_at={fail_at}: the created hooks directory must be rolled back"
        );

        let existing = scoped_dir("existing-hooks");
        fs::create_dir(existing.join("hooks")).expect("create user hooks dir");
        let mut gate = |index: usize, _name: &str| {
            if index == fail_at {
                Err(injected())
            } else {
                Ok(())
            }
        };
        install_claude_gated(&existing, &mut gate).expect_err("gate failure aborts");
        assert!(
            existing.join("hooks").is_dir(),
            "fail_at={fail_at}: a directory the user owns is never removed"
        );
    }
}

#[test]
fn a_hooks_directory_holding_another_file_survives_a_failed_install() {
    let dir = scoped_dir("created-hooks-busy");
    let intruder = dir.join("hooks").join("user-script.sh");
    let mut gate = |index: usize, _name: &str| {
        if index == 2 {
            fs::write(&intruder, "echo user\n").expect("write user file");
            return Err(injected());
        }
        Ok(())
    };

    install_claude_gated(&dir, &mut gate).expect_err("gate failure aborts");

    assert_eq!(
        fs::read_to_string(&intruder).expect("user file"),
        "echo user\n"
    );
}

/// Step names of a reinstall over an existing, unchanged install of `agent`.
///
/// An unchanged Codex `config.toml` adds verification steps, so the indices
/// differ per agent and are read from a dry run instead of assumed.
fn reseed_steps(agent: &RuntimeRef) -> Vec<String> {
    let dir = scoped_dir("reseed-steps");
    install_gated(agent, &dir, &mut |_index, _name| Ok(())).expect("seed install");
    let mut seen = Vec::new();
    install_gated(agent, &dir, &mut |_index, name| {
        seen.push(name.to_owned());
        Ok(())
    })
    .expect("reinstall for step names");
    seen
}

/// Index of the state-script step in `steps`.
fn state_script_index(steps: &[String]) -> usize {
    steps
        .iter()
        .position(|name| name == STATE_HOOK_INSTALL_NAME)
        .expect("the state script is a step")
}

/// A managed-script body one byte over the rollback snapshot limit.
fn oversized_script() -> Vec<u8> {
    vec![b'#'; super::PROVIDER_CONFIG_INSPECTION_LIMIT_BYTES + 1]
}

/// Path of the state hook script for `agent` inside `dir`.
fn state_script(agent: &RuntimeRef, dir: &Path) -> PathBuf {
    if *agent == RuntimeRef::claude() {
        dir.join("hooks").join(STATE_HOOK_INSTALL_NAME)
    } else {
        dir.join(STATE_HOOK_INSTALL_NAME)
    }
}

fn install_gated(
    agent: &RuntimeRef,
    dir: &Path,
    gate: super::StepGate<'_>,
) -> Result<(), ProtocolError> {
    if *agent == RuntimeRef::claude() {
        install_claude_gated(dir, gate).map(|_paths| ())
    } else {
        install_codex_gated(dir, gate).map(|_paths| ())
    }
}

#[test]
fn a_later_failure_restores_an_oversized_managed_script_exactly() {
    for agent in [RuntimeRef::claude(), RuntimeRef::codex()] {
        let steps = reseed_steps(&agent);
        for fail_at in (state_script_index(&steps) + 1)..steps.len() {
            let dir = scoped_dir("oversized-rollback");
            install_gated(&agent, &dir, &mut |_index, _name| Ok(())).expect("seed install");
            fs::write(state_script(&agent, &dir), oversized_script()).expect("oversize script");
            let before = tree_snapshot(&dir);
            let mut gate = |index: usize, _name: &str| {
                if index == fail_at {
                    Err(injected())
                } else {
                    Ok(())
                }
            };

            let error = install_gated(&agent, &dir, &mut gate).expect_err("gate failure aborts");

            assert_eq!(error.code, INJECTED_CODE, "{agent:?} fail_at={fail_at}");
            assert_eq!(
                tree_snapshot(&dir),
                before,
                "{agent:?} fail_at={fail_at}: the original script must survive byte for byte"
            );
        }
    }
}

#[test]
fn installing_over_an_oversized_managed_script_replaces_it_without_residue() {
    for agent in [RuntimeRef::claude(), RuntimeRef::codex()] {
        let dir = scoped_dir("oversized-replace");
        install_gated(&agent, &dir, &mut |_index, _name| Ok(())).expect("seed install");
        fs::write(state_script(&agent, &dir), oversized_script()).expect("oversize script");

        install_gated(&agent, &dir, &mut |_index, _name| Ok(())).expect("reinstall repairs it");

        assert_eq!(
            status_of(&dir, agent.clone()).state,
            IntegrationInstallState::Current,
            "{agent:?}"
        );
        let leftovers: Vec<_> = tree_snapshot(&dir)
            .into_iter()
            .filter(|(path, _mode, _content)| {
                path.file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|name| name.starts_with(".pohunek-integration-displaced-"))
            })
            .collect();
        assert!(leftovers.is_empty(), "{agent:?}: {leftovers:?}");
    }
}

#[test]
fn an_unrestorable_oversized_original_is_reported_and_kept_never_silently_lost() {
    for agent in [RuntimeRef::claude(), RuntimeRef::codex()] {
        let dir = scoped_dir("oversized-recovery");
        install_gated(&agent, &dir, &mut |_index, _name| Ok(())).expect("seed install");
        let script = state_script(&agent, &dir);
        fs::write(&script, oversized_script()).expect("oversize script");
        let last = reseed_steps(&agent).len() - 1;
        let mut gate = |index: usize, _name: &str| {
            if index == last {
                fs::remove_file(&script).expect("remove the rewritten script");
                fs::create_dir(&script).expect("shadow it with a directory");
                return Err(injected());
            }
            Ok(())
        };

        let error = install_gated(&agent, &dir, &mut gate).expect_err("failure aborts");

        assert_eq!(error.code, DESTINATION_COLLISION_CODE, "{agent:?}");
        let parent = script.parent().expect("script parent");
        let kept = fs::read_dir(parent)
            .expect("read script directory")
            .filter_map(Result::ok)
            .any(|entry| {
                entry.metadata().is_ok_and(|metadata| {
                    metadata.is_file() && metadata.len() == oversized_script().len() as u64
                })
            });
        assert!(
            kept,
            "{agent:?}: the original oversized file must remain on disk"
        );
    }
}

#[test]
fn a_rollback_that_cannot_put_an_original_back_requires_recovery_and_names_it() {
    for agent in [RuntimeRef::claude(), RuntimeRef::codex()] {
        let dir = scoped_dir("recovery-vanished");
        install_gated(&agent, &dir, &mut |_index, _name| Ok(())).expect("seed install");
        let script = state_script(&agent, &dir);
        let scripts_dir = script.parent().expect("script parent").to_path_buf();
        let fail_at = if agent == RuntimeRef::claude() {
            "settings.json"
        } else {
            "hooks.json"
        };
        // The quarantined original of the rewritten script vanishes, so the
        // rollback has nothing to move back.
        let vanish = scripts_dir.clone();
        let _hook = RaceHook::install(move |label, name| {
            if label == "write.committed" && name == STATE_HOOK_INSTALL_NAME {
                for entry in fs::read_dir(&vanish).expect("list").filter_map(Result::ok) {
                    if entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.starts_with(".pohunek-displaced-"))
                    {
                        fs::remove_file(entry.path()).expect("remove the quarantined original");
                    }
                }
            }
        });
        let mut gate = move |_index: usize, name: &str| {
            if name == fail_at {
                Err(injected())
            } else {
                Ok(())
            }
        };

        let error = install_gated(&agent, &dir, &mut gate).expect_err("failure aborts");

        assert_eq!(error.code, RECOVERY_REQUIRED_CODE, "{agent:?}");
        assert!(
            error.msg.starts_with("integration install failed"),
            "{}",
            error.msg
        );
        assert!(
            error.msg.contains("the original could not be moved back"),
            "{}",
            error.msg
        );
    }
}
