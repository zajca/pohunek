//! Removal and diagnosis tests for the Claude and Codex integrations.
//!
//! Every test runs natively on Linux and macOS; nothing here spawns a real
//! `python3`, so the macOS stub case is exercised by path fixtures alone.

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt as _};
use std::path::{Path, PathBuf};

use pohunek_paths::{PathEnv, Platform};
use protocol::{
    AgentKind, ErrorClass, IntegrationDoctorParams, IntegrationFindingCode as Code,
    IntegrationFindingSeverity as Severity, IntegrationInstallState, IntegrationUninstallReport,
    IntegrationUninstallState, ProtocolError,
};
use serde_json::{json, Value};

use super::commit::{DESTINATION_COLLISION_CODE, RECOVERY_REQUIRED_CODE};
use super::doctor::{
    classify_warning, diagnose, doctor_with, python_findings, socket_findings, MacosStub,
    PythonProbe, PythonState,
};
use super::tests::{explicit_status, read_json, temp_dir, tree_snapshot, with_config_dirs};
use super::uninstall::{uninstall_claude_gated, uninstall_codex_gated};
use super::{
    install_claude, install_codex, uninstall_claude, uninstall_codex, TrustedDir,
    CLAUDE_HOOK_ASSET, CODEX_HOOK_ASSET, INSTALL_IN_PROGRESS_CODE, INSTALL_LOCK_NAME,
    NOTIFY_HOOK_INSTALL_NAME, STATE_HOOK_INSTALL_NAME,
};

/// Error code raised by the injected gate failures below.
const INJECTED_CODE: &str = "injected_step_failure";

/// Secret-looking values planted in provider files.
const SENTINELS: [&str; 2] = ["sk-sentinel-removal-0001", "sk-sentinel-doctor-0002"];

/// File-type mask and directory bits of a Unix mode.
const FILE_TYPE_MASK: u32 = 0o170_000;
const DIRECTORY_TYPE: u32 = 0o040_000;

fn injected() -> ProtocolError {
    ProtocolError::new(ErrorClass::Runtime, INJECTED_CODE, "injected failure", None)
}

/// Regular files only, without the installer lock.
fn content_snapshot(root: &Path) -> Vec<(PathBuf, u32, Vec<u8>)> {
    tree_snapshot(root)
        .into_iter()
        .filter(|(path, mode, _content)| {
            path.file_name() != Some(std::ffi::OsStr::new(INSTALL_LOCK_NAME))
                && mode & FILE_TYPE_MASK != DIRECTORY_TYPE
        })
        .collect()
}

fn user_settings() -> Value {
    json!({
        "model": "user-model",
        "hooks": {
            "PreToolUse": [
                { "matcher": "*", "hooks": [{ "type": "command", "command": "echo pre" }] }
            ],
            "SessionStart": [
                { "matcher": "*", "hooks": [{ "type": "command", "command": "echo start" }] }
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

fn removed_names(report: &IntegrationUninstallReport) -> Vec<String> {
    report
        .removed_paths
        .iter()
        .map(|path| {
            Path::new(path)
                .file_name()
                .expect("file name")
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

#[test]
fn claude_uninstall_after_install_restores_user_settings_and_is_idempotent() {
    let dir = temp_dir("rm-claude");
    write_json(&dir.join("settings.json"), &user_settings());
    install_claude(&dir).expect("install");

    let report = uninstall_claude(&dir).expect("uninstall");

    assert_eq!(report.state, IntegrationUninstallState::Removed);
    assert_eq!(
        removed_names(&report),
        [STATE_HOOK_INSTALL_NAME, NOTIFY_HOOK_INSTALL_NAME]
    );
    assert_eq!(
        report.updated_paths,
        [dir.join("settings.json").display().to_string()]
    );
    assert!(report.preserved_paths.is_empty());
    assert_eq!(read_json(&dir.join("settings.json")), user_settings());
    assert!(!dir.join("hooks").join(STATE_HOOK_INSTALL_NAME).exists());
    assert!(!dir.join("hooks").join(NOTIFY_HOOK_INSTALL_NAME).exists());
    assert_eq!(
        explicit_status(&dir, AgentKind::Claude).state,
        IntegrationInstallState::NotInstalled
    );

    let settled = content_snapshot(&dir);
    let again = uninstall_claude(&dir).expect("second uninstall");
    assert_eq!(again.state, IntegrationUninstallState::NotInstalled);
    assert!(again.removed_paths.is_empty() && again.updated_paths.is_empty());
    assert_eq!(content_snapshot(&dir), settled);
}

#[test]
fn codex_uninstall_after_install_keeps_user_hooks_and_drops_only_managed_trust() {
    let dir = temp_dir("rm-codex");
    let user_hooks = json!({
        "hooks": {
            "SessionStart": [
                { "hooks": [{ "type": "command", "command": "echo codex-user" }] }
            ]
        }
    });
    write_json(&dir.join("hooks.json"), &user_hooks);
    fs::write(dir.join("config.toml"), "model = \"user-model\"\n").expect("seed config");
    install_codex(&dir).expect("install");

    let report = uninstall_codex(&dir).expect("uninstall");

    assert_eq!(report.state, IntegrationUninstallState::Removed);
    assert_eq!(report.updated_paths.len(), 2, "{:?}", report.updated_paths);
    assert_eq!(
        removed_names(&report),
        [STATE_HOOK_INSTALL_NAME, NOTIFY_HOOK_INSTALL_NAME]
    );
    assert_eq!(read_json(&dir.join("hooks.json")), user_hooks);
    let config: toml::Value =
        toml::from_str(&fs::read_to_string(dir.join("config.toml")).expect("read config"))
            .expect("config stays valid TOML");
    assert_eq!(config["model"].as_str(), Some("user-model"));
    assert!(
        config.get("hooks").is_none(),
        "managed trust records remain: {config}"
    );
    assert_eq!(
        config["features"]["hooks"].as_bool(),
        Some(true),
        "the user-visible feature flag is not the uninstaller's to change"
    );
    assert_eq!(
        explicit_status(&dir, AgentKind::Codex).state,
        IntegrationInstallState::NotInstalled
    );
    let settled = content_snapshot(&dir);
    assert_eq!(
        uninstall_codex(&dir).expect("second").state,
        IntegrationUninstallState::NotInstalled
    );
    assert_eq!(content_snapshot(&dir), settled);
}

#[test]
fn uninstall_after_upgrade_removes_an_older_marked_script() {
    let claude = temp_dir("rm-upgrade-claude");
    install_claude(&claude).expect("install");
    let state = claude.join("hooks").join(STATE_HOOK_INSTALL_NAME);
    fs::write(&state, CLAUDE_HOOK_ASSET.replace("VERSION=7", "VERSION=1")).expect("age script");
    install_claude(&claude).expect("upgrade");
    fs::write(&state, CLAUDE_HOOK_ASSET.replace("VERSION=7", "VERSION=2")).expect("age again");

    let report = uninstall_claude(&claude).expect("uninstall aged install");

    assert!(!state.exists(), "an older marked script is still owned");
    assert_eq!(report.state, IntegrationUninstallState::Removed);

    let codex = temp_dir("rm-upgrade-codex");
    install_codex(&codex).expect("install");
    fs::write(
        codex.join(STATE_HOOK_INSTALL_NAME),
        CODEX_HOOK_ASSET.replace("VERSION=7", "VERSION=1"),
    )
    .expect("age script");
    uninstall_codex(&codex).expect("uninstall aged Codex");
    assert!(!codex.join(STATE_HOOK_INSTALL_NAME).exists());
}

#[test]
fn foreign_entries_at_managed_paths_are_preserved_and_reported() {
    let dir = temp_dir("rm-foreign");
    install_codex(&dir).expect("install");
    let state = dir.join(STATE_HOOK_INSTALL_NAME);
    let notify = dir.join(NOTIFY_HOOK_INSTALL_NAME);
    fs::write(&state, "#!/bin/sh\necho user-owned\n").expect("replace with a foreign script");
    let target = dir.join("private-target");
    fs::write(&target, b"private").expect("write target");
    fs::remove_file(&notify).expect("remove notify");
    symlink(&target, &notify).expect("link notify to a target");

    let report = uninstall_codex(&dir).expect("uninstall");

    assert!(
        report.removed_paths.is_empty(),
        "{:?}",
        report.removed_paths
    );
    assert_eq!(
        report.preserved_paths,
        [state.display().to_string(), notify.display().to_string()]
    );
    assert_eq!(
        fs::read_to_string(&state).expect("foreign script"),
        "#!/bin/sh\necho user-owned\n"
    );
    assert_eq!(fs::read_link(&notify).expect("symlink kept"), target);
    assert_eq!(fs::read(&target).expect("target kept"), b"private");
    // Registration is still removed, so the agent no longer calls the scripts.
    assert!(!read_json(&dir.join("hooks.json"))
        .to_string()
        .contains("pohunek-agent-"));
}

#[test]
fn a_directory_or_fifo_at_a_managed_path_is_never_removed() {
    let dir = temp_dir("rm-special");
    install_claude(&dir).expect("install");
    let hooks = dir.join("hooks");
    let state = hooks.join(STATE_HOOK_INSTALL_NAME);
    let notify = hooks.join(NOTIFY_HOOK_INSTALL_NAME);
    fs::remove_file(&state).expect("remove state");
    fs::create_dir(&state).expect("directory at state path");
    fs::remove_file(&notify).expect("remove notify");
    let output = std::process::Command::new("mkfifo")
        .arg(&notify)
        .output()
        .expect("run mkfifo");
    assert!(output.status.success());

    let report = uninstall_claude(&dir).expect("uninstall");

    assert_eq!(report.preserved_paths.len(), 2);
    assert!(state.is_dir());
    assert!(fs::symlink_metadata(&notify)
        .expect("fifo")
        .file_type()
        .is_fifo_like());
}

trait FifoLike {
    fn is_fifo_like(&self) -> bool;
}

impl FifoLike for fs::FileType {
    fn is_fifo_like(&self) -> bool {
        use std::os::unix::fs::FileTypeExt as _;
        self.is_fifo()
    }
}

#[test]
fn uninstall_without_an_install_or_config_dir_changes_nothing() {
    let root = temp_dir("rm-absent");
    let missing = root.join("missing");
    assert_eq!(
        uninstall_claude(&missing).expect("absent").state,
        IntegrationUninstallState::NotInstalled
    );
    assert!(
        !missing.exists(),
        "an absent agent directory is never created"
    );

    let empty = root.join("empty");
    fs::create_dir(&empty).expect("create empty agent dir");
    let before = content_snapshot(&empty);
    assert_eq!(
        uninstall_codex(&empty).expect("uninstalled").state,
        IntegrationUninstallState::NotInstalled
    );
    assert_eq!(content_snapshot(&empty), before);

    let file = root.join("file");
    fs::write(&file, "").expect("write file");
    assert_eq!(
        uninstall_claude(&file).expect_err("not a directory").code,
        "integration_path_untrusted"
    );
}

/// Step names an uninstall of a fresh install runs, in order.
fn removal_steps(agent: &AgentKind) -> Vec<String> {
    let dir = temp_dir("rm-steps");
    let mut seen = Vec::new();
    let mut gate = |_index: usize, name: &str| {
        seen.push(name.to_owned());
        Ok(())
    };
    if *agent == AgentKind::Claude {
        install_claude(&dir).expect("install");
        uninstall_claude_gated(&dir, &mut gate).map(|_report| ())
    } else {
        install_codex(&dir).expect("install");
        uninstall_codex_gated(&dir, &mut gate).map(|_report| ())
    }
    .expect("uninstall for step names");
    seen
}

#[test]
fn removal_edits_registration_before_deleting_scripts() {
    assert_eq!(
        removal_steps(&AgentKind::Claude),
        [
            "settings.json",
            STATE_HOOK_INSTALL_NAME,
            NOTIFY_HOOK_INSTALL_NAME
        ]
    );
    assert_eq!(
        removal_steps(&AgentKind::Codex),
        [
            "hooks.json",
            "config.toml",
            STATE_HOOK_INSTALL_NAME,
            NOTIFY_HOOK_INSTALL_NAME
        ]
    );
}

#[test]
fn injected_failure_at_every_removal_step_restores_the_installed_tree() {
    for agent in [AgentKind::Claude, AgentKind::Codex] {
        for fail_at in 0..removal_steps(&agent).len() {
            let dir = temp_dir("rm-rollback");
            if agent == AgentKind::Claude {
                write_json(&dir.join("settings.json"), &user_settings());
                install_claude(&dir).expect("install");
            } else {
                fs::write(dir.join("config.toml"), "model = \"m\"\n").expect("seed");
                install_codex(&dir).expect("install");
            }
            let before = content_snapshot(&dir);
            let mut gate = |index: usize, _name: &str| {
                if index == fail_at {
                    Err(injected())
                } else {
                    Ok(())
                }
            };

            let error = if agent == AgentKind::Claude {
                uninstall_claude_gated(&dir, &mut gate).map(|_report| ())
            } else {
                uninstall_codex_gated(&dir, &mut gate).map(|_report| ())
            }
            .expect_err("gate failure aborts the removal");

            assert_eq!(error.code, INJECTED_CODE);
            assert_eq!(
                content_snapshot(&dir),
                before,
                "{agent:?} fail_at={fail_at} left the tree changed"
            );
            assert_eq!(
                explicit_status(&dir, agent.clone()).state,
                IntegrationInstallState::Current,
                "{agent:?} fail_at={fail_at}"
            );
        }
    }
}

#[test]
fn a_registration_edited_during_removal_is_a_collision_and_nothing_is_deleted() {
    let dir = temp_dir("rm-collision");
    install_claude(&dir).expect("install");
    let edit = json!({ "edited": "concurrently" });
    let mut gate = |_index: usize, name: &str| {
        if name == "settings.json" {
            write_json(&dir.join("settings.json"), &edit);
        }
        Ok(())
    };

    let error = uninstall_claude_gated(&dir, &mut gate).expect_err("collision aborts");

    assert_eq!(error.code, DESTINATION_COLLISION_CODE);
    assert_eq!(read_json(&dir.join("settings.json")), edit);
    assert!(dir.join("hooks").join(STATE_HOOK_INSTALL_NAME).exists());
    assert!(dir.join("hooks").join(NOTIFY_HOOK_INSTALL_NAME).exists());
}

#[test]
fn a_removal_rollback_that_finds_its_script_recreated_requires_recovery() {
    let dir = temp_dir("rm-recovery");
    install_claude(&dir).expect("install");
    let state = dir.join("hooks").join(STATE_HOOK_INSTALL_NAME);
    let mut gate = |_index: usize, name: &str| {
        if name == NOTIFY_HOOK_INSTALL_NAME {
            fs::write(&state, "recreated by someone else\n").expect("recreate script");
            return Err(injected());
        }
        Ok(())
    };

    let error = uninstall_claude_gated(&dir, &mut gate).expect_err("failure aborts");

    assert_eq!(error.code, RECOVERY_REQUIRED_CODE);
    assert!(
        error.msg.contains(&state.display().to_string()),
        "{}",
        error.msg
    );
    assert_eq!(
        fs::read_to_string(&state).expect("recreated file kept"),
        "recreated by someone else\n"
    );
}

#[test]
fn uninstall_serializes_with_installers_on_the_same_lock() {
    let dir = temp_dir("rm-lock");
    install_claude(&dir).expect("install");
    let before = content_snapshot(&dir);
    let holder = TrustedDir::open(&dir, "test root").expect("open");
    let lock = holder.lock_installer().expect("hold lock");

    let error = uninstall_claude(&dir).expect_err("second holder must not proceed");

    assert_eq!(error.code, INSTALL_IN_PROGRESS_CODE);
    assert_eq!(content_snapshot(&dir), before);
    drop(lock);
    uninstall_claude(&dir).expect("uninstall after release");
}

#[test]
fn profiles_are_removed_independently() {
    let a = temp_dir("rm-profile-a");
    let b = temp_dir("rm-profile-b");
    install_claude(&a).expect("install A");
    install_claude(&b).expect("install B");
    let b_before = content_snapshot(&b);

    uninstall_claude(&a).expect("uninstall A");

    assert_eq!(content_snapshot(&b), b_before);
    assert_eq!(
        explicit_status(&b, AgentKind::Claude).state,
        IntegrationInstallState::Current
    );
}

#[test]
fn explicit_agent_selection_and_unsupported_agents() {
    let claude = temp_dir("rm-select-claude");
    let codex = temp_dir("rm-select-codex");
    install_claude(&claude).expect("install Claude");
    install_codex(&codex).expect("install Codex");
    let codex_before = content_snapshot(&codex);

    let result = with_config_dirs(&claude, &codex, || super::uninstall(AgentKind::Claude))
        .expect("uninstall Claude only");
    assert_eq!(result.uninstalled.len(), 1);
    assert_eq!(result.uninstalled[0].agent, AgentKind::Claude);
    assert_eq!(content_snapshot(&codex), codex_before);

    let codex_result = with_config_dirs(&claude, &codex, || super::uninstall(AgentKind::Codex))
        .expect("uninstall Codex");
    assert_eq!(codex_result.uninstalled.len(), 1);
    assert_eq!(
        codex_result.uninstalled[0].state,
        IntegrationUninstallState::Removed
    );

    for agent in [AgentKind::Shell, AgentKind::Hermes] {
        let error = super::uninstall(agent).expect_err("unsupported agent");
        assert_eq!(error.code, "agent_not_installable");
    }
    let unknown = super::uninstall(AgentKind::Unknown("pi".to_owned())).expect_err("unknown agent");
    assert_eq!(unknown.code, "agent_kind_unsupported");
}

fn assert_no_sentinel(label: &str, text: &str) {
    for sentinel in SENTINELS {
        assert!(
            !text.contains(sentinel),
            "{label} leaked {sentinel}: {text}"
        );
    }
}

#[test]
fn removal_and_doctor_outputs_never_carry_provider_secrets() {
    let claude = temp_dir("rm-secret-claude");
    let codex = temp_dir("rm-secret-codex");
    write_json(
        &claude.join("settings.json"),
        &json!({ "env": { "KEY": SENTINELS[0] }, "hooks": {} }),
    );
    fs::write(
        codex.join("config.toml"),
        format!("api_key = \"{}\"\n", SENTINELS[1]),
    )
    .expect("seed config");
    install_claude(&claude).expect("install Claude");
    install_codex(&codex).expect("install Codex");
    let mut outputs = Vec::new();

    let doctor = with_config_dirs(&claude, &codex, || {
        doctor_with(IntegrationDoctorParams::default(), &[], &[])
    })
    .expect("doctor");
    outputs.push(serde_json::to_string(&doctor).expect("doctor json"));

    fs::write(
        codex.join("config.toml"),
        format!("api_key = \"{}\" trailing garbage\n", SENTINELS[1]),
    )
    .expect("corrupt config");
    fs::write(
        claude.join("settings.json"),
        format!("{{\"env\": \"{}\" oops", SENTINELS[0]),
    )
    .expect("corrupt settings");
    for error in [
        uninstall_codex(&codex).expect_err("malformed TOML"),
        uninstall_claude(&claude).expect_err("malformed JSON"),
    ] {
        outputs.push(format!(
            "{} {} {:?} {error:?}",
            error.code, error.msg, error.recover
        ));
    }
    let broken = with_config_dirs(&claude, &codex, || {
        doctor_with(IntegrationDoctorParams::default(), &[], &[])
    })
    .expect("doctor over corrupt files");
    outputs.push(serde_json::to_string(&broken).expect("doctor json"));

    for (index, text) in outputs.iter().enumerate() {
        assert_no_sentinel(&format!("output {index}"), text);
    }
}

fn find(findings: &[protocol::IntegrationFinding], code: Code) -> &protocol::IntegrationFinding {
    findings
        .iter()
        .find(|finding| finding.code == code)
        .unwrap_or_else(|| panic!("missing {code:?} in {findings:?}"))
}

fn fresh_claude() -> PathBuf {
    let dir = temp_dir("doctor-claude");
    install_claude(&dir).expect("install Claude");
    dir
}

fn fresh_codex() -> PathBuf {
    let dir = temp_dir("doctor-codex");
    install_codex(&dir).expect("install Codex");
    dir
}

fn write_file(path: PathBuf, body: impl AsRef<[u8]>) {
    fs::write(path, body).expect("write fixture file");
}

fn remove_file(path: PathBuf) {
    fs::remove_file(path).expect("remove fixture file");
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set mode");
}

#[test]
fn doctor_maps_every_claude_drift_cause_to_its_finding() {
    type Mutation = fn(&Path);
    let causes: [(&str, Mutation, Code); 11] = [
        (
            "state missing",
            |d| remove_file(d.join("hooks").join(STATE_HOOK_INSTALL_NAME)),
            Code::AssetMissing,
        ),
        (
            "notify modified",
            |d| {
                write_file(
                    d.join("hooks").join(NOTIFY_HOOK_INSTALL_NAME),
                    "# modified\n",
                );
            },
            Code::AssetModified,
        ),
        (
            "old version",
            |d| {
                write_file(
                    d.join("hooks").join(STATE_HOOK_INSTALL_NAME),
                    CLAUDE_HOOK_ASSET.replace("VERSION=7", "VERSION=1"),
                );
            },
            Code::AssetModified,
        ),
        (
            "oversized",
            |d| {
                write_file(
                    d.join("hooks").join(STATE_HOOK_INSTALL_NAME),
                    vec![b'#'; super::MANAGED_ASSET_INSPECTION_LIMIT_BYTES + 1],
                );
            },
            Code::AssetModified,
        ),
        (
            "world writable",
            |d| set_mode(&d.join("hooks").join(STATE_HOOK_INSTALL_NAME), 0o777),
            Code::AssetUnsafe,
        ),
        (
            "mode drift",
            |d| set_mode(&d.join("hooks").join(STATE_HOOK_INSTALL_NAME), 0o600),
            Code::AssetUnsafe,
        ),
        (
            "unsafe parent",
            |d| set_mode(&d.join("hooks"), 0o775),
            Code::AssetUnsafe,
        ),
        (
            "malformed settings",
            |d| write_file(d.join("settings.json"), "{ nope"),
            Code::ProviderConfigInvalid,
        ),
        (
            "array settings",
            |d| write_file(d.join("settings.json"), "[]"),
            Code::ProviderConfigInvalid,
        ),
        (
            "registration gone",
            |d| write_file(d.join("settings.json"), "{}"),
            Code::RegistrationDrift,
        ),
        (
            "settings missing",
            |d| remove_file(d.join("settings.json")),
            Code::RegistrationDrift,
        ),
    ];
    for (name, mutate, expected) in causes {
        let dir = fresh_claude();
        mutate(&dir);

        let doctor = diagnose(explicit_status(&dir, AgentKind::Claude), &[]);

        assert!(!doctor.ok, "{name}");
        let finding = find(&doctor.findings, expected);
        assert_eq!(finding.severity, Severity::Error, "{name}");
        let remediation = finding.remediation.as_deref().expect("remediation");
        assert!(
            remediation.contains("pohunek integration install --agent claude"),
            "{name}: {remediation}"
        );
        assert!(
            doctor.findings.iter().all(|f| f.code != Code::InstallDrift),
            "{name}: {:?}",
            doctor.findings
        );
    }
}

#[test]
fn doctor_maps_every_codex_drift_cause_to_its_finding() {
    type Mutation = fn(&Path);
    let causes: [(&str, Mutation, Code); 6] = [
        (
            "malformed config",
            |d| write_file(d.join("config.toml"), "= nope"),
            Code::ProviderConfigInvalid,
        ),
        (
            "feature disabled",
            |d| write_file(d.join("config.toml"), "[features]\nhooks = false\n"),
            Code::CodexHooksFeatureDisabled,
        ),
        (
            "trust missing",
            |d| write_file(d.join("config.toml"), "[features]\nhooks = true\n"),
            Code::CodexTrustDrift,
        ),
        (
            "config missing",
            |d| remove_file(d.join("config.toml")),
            Code::RegistrationDrift,
        ),
        (
            "hooks.json malformed",
            |d| write_file(d.join("hooks.json"), "{ nope"),
            Code::ProviderConfigInvalid,
        ),
        (
            "asset symlink",
            |d| {
                let hook = d.join(STATE_HOOK_INSTALL_NAME);
                remove_file(hook.clone());
                symlink(d.join("config.toml"), &hook).expect("link");
            },
            Code::AssetUnsafe,
        ),
    ];
    for (name, mutate, expected) in causes {
        let dir = fresh_codex();
        mutate(&dir);

        let doctor = diagnose(explicit_status(&dir, AgentKind::Codex), &[]);

        assert!(!doctor.ok, "{name}");
        let finding = find(&doctor.findings, expected);
        assert_eq!(finding.severity, Severity::Error, "{name}");
        assert!(finding.remediation.is_some(), "{name}");
        assert!(
            doctor.findings.iter().all(|f| f.code != Code::InstallDrift),
            "{name}: {:?}",
            doctor.findings
        );
    }
}

#[test]
fn doctor_reports_config_root_failures_and_an_unresolvable_root() {
    let root = temp_dir("doctor-root");
    let file = root.join("file");
    fs::write(&file, "").expect("write file");

    let doctor = diagnose(explicit_status(&file, AgentKind::Codex), &[]);

    assert!(!doctor.ok);
    assert_eq!(
        find(&doctor.findings, Code::ConfigRootInvalid).severity,
        Severity::Error
    );
    let unresolved = classify_warning(
        "agent config directory could not be resolved (agent_config_dir_invalid)",
        "claude",
        protocol::IntegrationRecovery::RepairConfiguration,
    );
    assert_eq!(unresolved.code, Code::ConfigRootInvalid);
}

#[test]
fn doctor_distinguishes_an_absent_optional_agent_from_absent_hooks_and_a_healthy_install() {
    let root = temp_dir("doctor-optional");
    let absent = diagnose(
        explicit_status(&root.join("missing"), AgentKind::Claude),
        &[],
    );
    assert!(absent.ok);
    assert_eq!(absent.findings.len(), 1);
    assert_eq!(absent.findings[0].code, Code::AgentNotInstalled);
    assert_eq!(absent.findings[0].severity, Severity::Info);

    let bare = root.join("bare");
    fs::create_dir(&bare).expect("create bare dir");
    let runtime_error = python_findings(&PythonProbe {
        search_dirs: vec![],
        macos_stub: None,
    });
    let hooks_absent = diagnose(explicit_status(&bare, AgentKind::Codex), &runtime_error);
    assert!(
        hooks_absent.ok,
        "runtime findings do not apply to an absent install"
    );
    assert_eq!(hooks_absent.findings.len(), 1);
    assert_eq!(hooks_absent.findings[0].code, Code::HooksNotInstalled);
    assert_eq!(hooks_absent.findings[0].severity, Severity::Info);

    let healthy = diagnose(explicit_status(&fresh_codex(), AgentKind::Codex), &[]);
    assert!(healthy.ok);
    assert!(healthy.findings.is_empty(), "{:?}", healthy.findings);

    let missing_runtime = diagnose(
        explicit_status(&fresh_claude(), AgentKind::Claude),
        &runtime_error,
    );
    assert!(
        missing_runtime.ok,
        "the python3 note is informational and never fails the doctor"
    );
    assert_eq!(missing_runtime.findings[0].code, Code::HookRuntimeMissing);
    assert_eq!(missing_runtime.findings[0].severity, Severity::Info);
}

/// Writes an executable `python3` that would record a marker if it ever ran.
fn write_probe_script(dir: &Path, marker: &Path) -> PathBuf {
    fs::create_dir_all(dir).expect("create bin dir");
    let script = dir.join("python3");
    fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .expect("write probe script");
    set_mode(&script, 0o755);
    script
}

#[test]
fn python_probe_reports_the_first_match_informationally_and_never_runs_it() {
    let root = temp_dir("doctor-python");
    let marker = root.join("was-executed");
    let stub_dir = root.join("usr-bin");
    let stub = write_probe_script(&stub_dir, &marker);
    let real_dir = root.join("homebrew");
    let real = write_probe_script(&real_dir, &marker);
    let developer = root.join("clt").join("python3");
    let probe = |search_dirs: Vec<PathBuf>| PythonProbe {
        search_dirs,
        macos_stub: Some(MacosStub {
            stub: stub.clone(),
            developer_pythons: vec![developer.clone()],
        }),
    };
    let only = |findings: Vec<protocol::IntegrationFinding>| {
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(
            findings[0].severity,
            Severity::Info,
            "the python3 note never fails the doctor"
        );
        findings.into_iter().next().expect("one finding")
    };

    // The first match decides and is named; later entries do not matter.
    let real_first = only(python_findings(&probe(vec![
        real_dir.clone(),
        stub_dir.clone(),
    ])));
    assert_eq!(real_first.code, Code::HookRuntimePythonFound);
    assert!(real_first.summary.contains(&real.display().to_string()));
    assert!(real_first.summary.contains("agent's own PATH"));

    // Stub first (or alone) without developer tools: the informational shim note.
    for dirs in [
        vec![stub_dir.clone(), real_dir.clone()],
        vec![stub_dir.clone()],
    ] {
        let note = only(python_findings(&probe(dirs)));
        assert_eq!(note.code, Code::HookRuntimeMacosShim);
        let fix = note.remediation.expect("remediation");
        assert!(fix.contains("agent's PATH"), "{fix}");
        assert!(fix.contains("xcode-select -p"), "{fix}");
    }

    // Developer tools behind the stub make it a normal interpreter.
    write_probe_script(developer.parent().expect("developer dir"), &marker);
    assert_eq!(
        only(python_findings(&probe(vec![stub_dir.clone()]))).code,
        Code::HookRuntimePythonFound
    );
    fs::remove_file(&developer).expect("remove developer python");

    // Nothing usable: non-executable, not-a-directory, absent, relative, and
    // empty entries are all skipped.
    let plain = root.join("plain");
    fs::create_dir(&plain).expect("create plain dir");
    fs::write(plain.join("python3"), "not executable").expect("write non-executable");
    let a_file = root.join("a-file");
    fs::write(&a_file, "").expect("write file");
    let none = only(python_findings(&probe(vec![
        plain,
        a_file,
        root.join("absent"),
        PathBuf::from(""),
        PathBuf::from("relative/bin"),
    ])));
    assert_eq!(none.code, Code::HookRuntimeMissing);
    assert!(none
        .remediation
        .expect("remediation")
        .contains("agent's PATH"));

    // Off macOS any executable python3 is simply the first match.
    let no_stub_platform = PythonProbe {
        search_dirs: vec![stub_dir],
        macos_stub: None,
    };
    assert_eq!(
        only(python_findings(&no_stub_platform)).code,
        Code::HookRuntimePythonFound
    );

    assert!(!marker.exists(), "the probe must never execute python3");
}

fn env_with_runtime(runtime: &Path) -> PathEnv {
    PathEnv {
        xdg_runtime_dir: Some(runtime.as_os_str().to_owned()),
        home: Some("/Users/fixture".into()),
        ..PathEnv::default()
    }
}

#[test]
fn socket_findings_reflect_the_daemon_and_worker_path_limits() {
    let short = Path::new("/private/tmp/ph-short");
    assert!(socket_findings(Platform::MacOs, 501, &env_with_runtime(short)).is_empty());

    let default_env = PathEnv {
        home: Some("/Users/fixture".into()),
        ..PathEnv::default()
    };
    assert!(socket_findings(Platform::MacOs, 501, &default_env).is_empty());

    let daemon_too_long = PathBuf::from(format!("/private/tmp/{}", "d".repeat(100)));
    let findings = socket_findings(Platform::MacOs, 501, &env_with_runtime(&daemon_too_long));
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].code, Code::HookSocketPathInvalid);
    assert!(findings[0]
        .remediation
        .as_deref()
        .expect("remediation")
        .contains("XDG_RUNTIME_DIR"));

    let worker_only = PathBuf::from(format!("/private/tmp/{}", "w".repeat(45)));
    let findings = socket_findings(Platform::MacOs, 501, &env_with_runtime(&worker_only));
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(
        findings[0].summary.contains("worker"),
        "{}",
        findings[0].summary
    );

    let no_runtime = PathEnv::default();
    assert_eq!(
        socket_findings(Platform::Linux, 1000, &no_runtime)[0].code,
        Code::HookSocketPathInvalid
    );
}

#[test]
fn doctor_selects_agents_and_rejects_unsupported_ones() {
    let claude = fresh_claude();
    let codex = temp_dir("doctor-select-codex");
    let only_claude = with_config_dirs(&claude, &codex, || {
        doctor_with(
            IntegrationDoctorParams {
                agent: Some(AgentKind::Claude),
            },
            &[],
            &[],
        )
    })
    .expect("doctor Claude");
    assert_eq!(only_claude.agents.len(), 1);
    assert!(only_claude.ok);

    let both = with_config_dirs(&claude, &codex, || {
        doctor_with(IntegrationDoctorParams::default(), &[], &[])
    })
    .expect("doctor both");
    assert_eq!(both.agents.len(), 2);

    for agent in [AgentKind::Shell, AgentKind::Hermes] {
        let error = doctor_with(IntegrationDoctorParams { agent: Some(agent) }, &[], &[])
            .expect_err("unsupported");
        assert_eq!(error.code, "agent_not_installable");
    }
    assert_eq!(
        doctor_with(
            IntegrationDoctorParams {
                agent: Some(AgentKind::Unknown("pi".to_owned())),
            },
            &[],
            &[],
        )
        .expect_err("unknown")
        .code,
        "agent_kind_unsupported"
    );
}

#[test]
fn doctor_public_entry_point_runs_against_the_process_environment() {
    let claude = fresh_claude();
    let codex = temp_dir("doctor-public");
    let result = with_config_dirs(&claude, &codex, || {
        super::doctor(IntegrationDoctorParams::default())
    })
    .expect("doctor");
    assert_eq!(result.agents.len(), 2);
    assert_eq!(
        result.agents[1].status.as_ref().expect("status").state,
        IntegrationInstallState::NotInstalled
    );
}

#[test]
fn an_oversized_owned_script_is_preserved_by_uninstall_and_survives_a_failed_removal() {
    for agent in [AgentKind::Claude, AgentKind::Codex] {
        let dir = temp_dir("rm-oversized");
        let script = if agent == AgentKind::Claude {
            install_claude(&dir).expect("install");
            dir.join("hooks").join(STATE_HOOK_INSTALL_NAME)
        } else {
            install_codex(&dir).expect("install");
            dir.join(STATE_HOOK_INSTALL_NAME)
        };
        let body = vec![b'#'; super::PROVIDER_CONFIG_INSPECTION_LIMIT_BYTES + 1];
        fs::write(&script, &body).expect("oversize script");
        let before = content_snapshot(&dir);
        let mut gate = |index: usize, _name: &str| {
            if index == removal_steps(&agent).len() - 1 {
                Err(injected())
            } else {
                Ok(())
            }
        };
        let error = if agent == AgentKind::Claude {
            uninstall_claude_gated(&dir, &mut gate).map(|_report| ())
        } else {
            uninstall_codex_gated(&dir, &mut gate).map(|_report| ())
        }
        .expect_err("gate failure aborts");
        assert_eq!(error.code, INJECTED_CODE);
        assert_eq!(
            content_snapshot(&dir),
            before,
            "{agent:?}: rollback is exact"
        );

        let report = if agent == AgentKind::Claude {
            uninstall_claude(&dir)
        } else {
            uninstall_codex(&dir)
        }
        .expect("uninstall");
        assert!(
            report
                .preserved_paths
                .contains(&script.display().to_string()),
            "{agent:?}: a script that cannot be verified as owned is left alone"
        );
        assert_eq!(fs::read(&script).expect("script kept"), body, "{agent:?}");
    }
}

/// Installs a race hook for the current test thread and removes it on drop.
pub(super) struct RaceHook;

impl RaceHook {
    pub(super) fn install(hook: impl FnMut(&str, &str) + 'static) -> Self {
        super::commit::RACE_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        Self
    }
}

impl Drop for RaceHook {
    fn drop(&mut self) {
        super::commit::RACE_HOOK.with(|slot| *slot.borrow_mut() = None);
    }
}

/// Atomically puts an unmarked file with the same mode at `path`.
fn swap_in_unmarked(path: &Path) {
    let replacement = path.with_extension("swap");
    fs::write(&replacement, "#!/bin/sh\necho user-owned\n").expect("write unmarked file");
    set_mode(&replacement, 0o755);
    fs::rename(&replacement, path).expect("swap the file atomically");
}

#[test]
fn removal_judges_ownership_on_the_inode_it_deletes() {
    for point in ["remove_owned.inspected", "remove_owned.identified"] {
        let dir = temp_dir("rm-toctou");
        install_claude(&dir).expect("install");
        let state = dir.join("hooks").join(STATE_HOOK_INSTALL_NAME);
        let target = state.clone();
        let _hook = RaceHook::install(move |label, name| {
            if label == point && name == STATE_HOOK_INSTALL_NAME {
                swap_in_unmarked(&target);
            }
        });

        let outcome = uninstall_claude(&dir);

        assert_eq!(
            fs::read_to_string(&state).expect("the swapped-in file must survive"),
            "#!/bin/sh\necho user-owned\n",
            "{point}: an unmarked file was deleted"
        );
        match (point, outcome) {
            ("remove_owned.inspected", Ok(report)) => {
                assert!(
                    report
                        .preserved_paths
                        .contains(&state.display().to_string()),
                    "{point}: {report:?}"
                );
            }
            ("remove_owned.identified", Err(error)) => {
                assert_eq!(error.code, DESTINATION_COLLISION_CODE, "{point}");
                assert!(
                    read_json(&dir.join("settings.json"))
                        .to_string()
                        .contains("pohunek-agent-"),
                    "{point}: the registration edit is rolled back"
                );
            }
            (point, other) => panic!("{point}: unexpected outcome {other:?}"),
        }
        let residue = tree_snapshot(&dir)
            .into_iter()
            .any(|(path, _mode, _content)| {
                path.file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .is_some_and(|name| name.starts_with(".pohunek-integration-rollback-"))
            });
        assert!(!residue, "{point}: quarantine residue");
    }
}

#[test]
fn a_provider_file_swapped_after_the_check_is_a_collision_not_a_clobber() {
    let dir = temp_dir("rm-swap-provider");
    write_json(&dir.join("settings.json"), &user_settings());
    install_claude(&dir).expect("install");
    let settings = dir.join("settings.json");
    let swap = settings.clone();
    let user_edit = json!({ "edited": "atomically, same mode" });
    let payload = user_edit.clone();
    let _hook = RaceHook::install(move |label, name| {
        if label == "write.decided" && name == "settings.json" {
            let replacement = swap.with_extension("swap");
            write_json(&replacement, &payload);
            set_mode(&replacement, 0o600);
            fs::rename(&replacement, &swap).expect("swap settings atomically");
        }
    });

    let error = uninstall_claude(&dir).expect_err("swap must be detected");

    assert_eq!(error.code, DESTINATION_COLLISION_CODE);
    assert_eq!(
        read_json(&settings),
        user_edit,
        "the user's file is untouched"
    );
    assert!(dir.join("hooks").join(STATE_HOOK_INSTALL_NAME).exists());
}

#[test]
fn a_mode_only_change_between_load_and_commit_is_a_collision() {
    for agent in [AgentKind::Claude, AgentKind::Codex] {
        let dir = temp_dir("rm-mode-only");
        let registration = if agent == AgentKind::Claude {
            install_claude(&dir).expect("install");
            dir.join("settings.json")
        } else {
            install_codex(&dir).expect("install");
            dir.join("hooks.json")
        };
        let name = registration
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .expect("registration file name")
            .to_owned();
        let target = registration.clone();
        let mut gate = move |_index: usize, step: &str| {
            if step == name {
                set_mode(&target, 0o640);
            }
            Ok(())
        };

        let error = if agent == AgentKind::Claude {
            uninstall_claude_gated(&dir, &mut gate).map(|_report| ())
        } else {
            uninstall_codex_gated(&dir, &mut gate).map(|_report| ())
        }
        .expect_err("a permission change is a collision");

        assert_eq!(error.code, DESTINATION_COLLISION_CODE, "{agent:?}");
        let mode = fs::metadata(&registration)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(
            mode, 0o640,
            "{agent:?}: the new permissions must not be reverted"
        );
    }
}

fn trust_hash(command: &str) -> String {
    super::codex_command_hook_trusted_hash(
        super::CODEX_SESSION_START_TRUST_EVENT,
        command,
        super::HOOK_TIMEOUT_SECS,
        None,
    )
    .expect("trust hash")
}

fn trust_table(hooks_path: &Path, group: usize, hash: &str) -> String {
    format!(
        "\n[hooks.state.{}]\ntrusted_hash = \"{hash}\"\n",
        super::toml_basic_string(&super::codex_hook_trust_key(
            hooks_path,
            super::CODEX_SESSION_START_TRUST_EVENT,
            group,
            0,
        ))
    )
}

#[test]
fn install_and_uninstall_keep_trust_records_of_the_users_own_hooks() {
    let dir = temp_dir("rm-user-trust");
    let hooks_path = dir.join("hooks.json");
    let user_before = "echo user-before";
    let user_after = "echo user-after";
    write_json(
        &hooks_path,
        &json!({ "hooks": { "SessionStart": [
            { "hooks": [{ "type": "command", "command": user_before }] }
        ]}}),
    );
    let before_hash = trust_hash(user_before);
    let after_hash = trust_hash(user_after);
    fs::write(
        dir.join("config.toml"),
        trust_table(&hooks_path, 0, &before_hash),
    )
    .expect("seed user trust");

    install_codex(&dir).expect("install keeps the user's record");
    assert_eq!(
        explicit_status(&dir, AgentKind::Codex).state,
        IntegrationInstallState::Current,
        "a user's trust record is not a stale managed key"
    );
    let installed = fs::read_to_string(dir.join("config.toml")).expect("read config");
    assert!(installed.contains(&before_hash), "{installed}");

    // A user hook appended after the managed ones, approved at its position.
    let mut hooks = read_json(&hooks_path);
    hooks["hooks"]["SessionStart"]
        .as_array_mut()
        .expect("SessionStart groups")
        .push(json!({ "hooks": [{ "type": "command", "command": user_after }] }));
    let after_group = hooks["hooks"]["SessionStart"]
        .as_array()
        .expect("groups")
        .len()
        - 1;
    write_json(&hooks_path, &hooks);
    let mut config = fs::read_to_string(dir.join("config.toml")).expect("read config");
    config.push_str(&trust_table(&hooks_path, after_group, &after_hash));
    fs::write(dir.join("config.toml"), config).expect("seed second user trust");

    let report = uninstall_codex(&dir).expect("uninstall");

    assert_eq!(report.state, IntegrationUninstallState::Removed);
    let remaining = fs::read_to_string(dir.join("config.toml")).expect("read config");
    assert!(
        remaining.contains(&before_hash),
        "hook before ours lost its record: {remaining}"
    );
    assert!(
        remaining.contains(&after_hash),
        "hook after ours lost its record: {remaining}"
    );
    let managed_hash = trust_hash(&super::hook_command(
        &dir.join(STATE_HOOK_INSTALL_NAME),
        super::HOOK_ACTION,
    ));
    assert!(
        !remaining.contains(&managed_hash),
        "managed record survived: {remaining}"
    );
    assert!(read_json(&hooks_path).to_string().contains(user_after));
}

fn dangling_link(root: &Path, name: &str) -> PathBuf {
    let link = root.join(name);
    symlink(root.join("nowhere"), &link).expect("create dangling symlink");
    link
}

#[test]
fn a_dangling_config_symlink_is_a_failure_for_status_doctor_install_and_uninstall() {
    let root = temp_dir("rm-dangling");
    for agent in [AgentKind::Claude, AgentKind::Codex] {
        let link = dangling_link(&root, &format!("{}-link", agent.as_wire()));
        let status = explicit_status(&link, agent.clone());
        assert!(!status.available, "{agent:?}");
        assert_eq!(status.state, IntegrationInstallState::Outdated, "{agent:?}");
        assert_eq!(
            status.recovery,
            protocol::IntegrationRecovery::RepairConfiguration,
            "{agent:?}"
        );
        assert!(
            status.warnings.iter().any(|w| w.contains("symlink")),
            "{agent:?}"
        );

        let doctor = diagnose(status, &[]);
        assert!(
            !doctor.ok,
            "{agent:?}: doctor must not call a symlinked root absent"
        );
        assert_eq!(
            find(&doctor.findings, Code::ConfigRootInvalid).severity,
            Severity::Error
        );

        let (install, uninstall) = if agent == AgentKind::Claude {
            (
                install_claude(&link).map(|_paths| ()),
                uninstall_claude(&link).map(|_report| ()),
            )
        } else {
            (
                install_codex(&link).map(|_paths| ()),
                uninstall_codex(&link).map(|_report| ()),
            )
        };
        assert_eq!(
            install.expect_err("install refuses").code,
            "integration_path_untrusted"
        );
        assert_eq!(
            uninstall.expect_err("uninstall refuses").code,
            "integration_path_untrusted"
        );
    }
}

#[test]
fn a_dangling_symlink_through_env_overrides_and_the_default_dir_is_a_failure() {
    let root = temp_dir("rm-dangling-env");
    let claude_link = dangling_link(&root, "claude-override");
    let codex_link = dangling_link(&root, "codex-override");

    let overridden = with_config_dirs(&claude_link, &codex_link, || {
        super::status(protocol::IntegrationStatusParams { agent: None })
    })
    .expect("status");
    for report in &overridden.agents {
        assert_eq!(
            report.state,
            IntegrationInstallState::Outdated,
            "{:?}",
            report.agent
        );
    }
    assert_eq!(
        with_config_dirs(&claude_link, &codex_link, || super::install(None))
            .expect_err("install refuses")
            .code,
        "integration_path_untrusted"
    );
    for agent in [AgentKind::Claude, AgentKind::Codex] {
        assert_eq!(
            with_config_dirs(&claude_link, &codex_link, || super::uninstall(
                agent.clone()
            ))
            .expect_err("uninstall refuses")
            .code,
            "integration_path_untrusted"
        );
    }

    let home = temp_dir("rm-dangling-home");
    symlink(home.join("nowhere"), home.join(".claude")).expect("dangling default Claude dir");
    symlink(home.join("nowhere"), home.join(".codex")).expect("dangling default Codex dir");
    let default = super::tests::with_status_env(None, None, Some(&home), || {
        super::status(protocol::IntegrationStatusParams { agent: None })
    })
    .expect("status");
    for report in &default.agents {
        assert_eq!(
            report.state,
            IntegrationInstallState::Outdated,
            "{:?}",
            report.agent
        );
        assert_eq!(
            report.recovery,
            protocol::IntegrationRecovery::RepairConfiguration
        );
    }
}

/// Names of quarantine entries the installer leaves behind, anywhere under `dir`.
fn quarantine_entries(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for directory in [dir.to_path_buf(), dir.join("hooks")] {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            let is_quarantine = name
                .to_str()
                .is_some_and(|name| name.starts_with(".pohunek-") && name != INSTALL_LOCK_NAME);
            if is_quarantine {
                found.push(entry.path());
            }
        }
    }
    found.sort();
    found
}

#[test]
fn a_foreign_change_after_the_decision_is_a_collision_and_stays_intact() {
    type Foreign = fn(&Path);
    let changes: [(&str, Foreign); 3] = [
        ("in-place write to the same inode", |settings| {
            fs::write(settings, "{\"edited\":true}").expect("write in place");
        }),
        ("permission change only", |settings| {
            set_mode(settings, 0o640);
        }),
        ("atomic swap keeping content and mode", |settings| {
            let replacement = settings.with_extension("swap");
            fs::copy(settings, &replacement).expect("copy settings");
            set_mode(&replacement, 0o600);
            fs::rename(&replacement, settings).expect("swap settings");
        }),
    ];
    for (name, change) in changes {
        let dir = temp_dir("rm-decided");
        write_json(&dir.join("settings.json"), &user_settings());
        install_claude(&dir).expect("install");
        let settings = dir.join("settings.json");
        let scripts_before: Vec<_> = content_snapshot(&dir)
            .into_iter()
            .filter(|(path, _mode, _content)| path.starts_with("hooks"))
            .collect();
        let target = settings.clone();
        let _hook = RaceHook::install(move |label, step| {
            if label == "write.decided" && step == "settings.json" {
                change(&target);
            }
        });

        let error = install_claude(&dir).expect_err("the foreign change must be detected");

        assert_eq!(error.code, DESTINATION_COLLISION_CODE, "{name}");
        let after: Vec<_> = content_snapshot(&dir)
            .into_iter()
            .filter(|(path, _mode, _content)| path.starts_with("hooks"))
            .collect();
        assert_eq!(
            after, scripts_before,
            "{name}: earlier steps are rolled back exactly"
        );
        assert!(
            quarantine_entries(&dir).is_empty(),
            "{name}: {:?}",
            quarantine_entries(&dir)
        );
        let text = fs::read_to_string(&settings).expect("read settings");
        match name {
            "in-place write to the same inode" => assert_eq!(text, "{\"edited\":true}"),
            "permission change only" => {
                let mode = fs::metadata(&settings)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o7777;
                assert_eq!(mode, 0o640, "the foreign permission change is kept");
            }
            _ => assert!(text.contains("user-model"), "{name}"),
        }
    }
}

/// Runs `remove` with a cleanup fault chosen by destination name.
struct CleanupFaultGuard;

impl CleanupFaultGuard {
    fn install(fault: impl FnMut(&str) -> Option<super::commit::CleanupFault> + 'static) -> Self {
        super::commit::CLEANUP_FAULT.with(|slot| *slot.borrow_mut() = Some(Box::new(fault)));
        Self
    }
}

impl Drop for CleanupFaultGuard {
    fn drop(&mut self) {
        super::commit::CLEANUP_FAULT.with(|slot| *slot.borrow_mut() = None);
    }
}

#[test]
fn a_cleanup_that_cannot_delete_an_original_reports_it_and_never_undoes_the_install() {
    use super::commit::CleanupFault;

    let dir = temp_dir("cleanup-unlink");
    write_json(&dir.join("settings.json"), &user_settings());
    install_claude(&dir).expect("seed install");
    let settings_before = fs::read_to_string(dir.join("settings.json")).expect("read settings");
    write_json(
        &dir.join("settings.json"),
        &read_json(&dir.join("settings.json")),
    );
    let original = fs::read_to_string(dir.join("settings.json")).expect("read original");
    let _fault = CleanupFaultGuard::install(|name| {
        (name == "settings.json").then_some(CleanupFault::Unlink)
    });

    let paths = install_claude(&dir).expect("a committed install is a success");

    assert_eq!(
        paths.cleanup_incomplete.len(),
        1,
        "{:?}",
        paths.cleanup_incomplete
    );
    assert!(
        paths.cleanup_incomplete[0].contains("left behind"),
        "{:?}",
        paths.cleanup_incomplete
    );
    let left = quarantine_entries(&dir);
    assert_eq!(left.len(), 1, "only the failed item remains: {left:?}");
    assert_eq!(
        fs::read_to_string(&left[0]).expect("the original is intact"),
        original,
        "no data is lost"
    );
    assert!(settings_before.contains("user-model"));
    assert_eq!(
        explicit_status(&dir, AgentKind::Claude).state,
        IntegrationInstallState::Current,
        "the new install is active"
    );
    assert!(
        !quarantine_entries(&dir.join("hooks"))
            .iter()
            .any(|p| p.starts_with(dir.join("hooks"))),
        "the other originals were still cleaned up"
    );
    let doctor = doctor_with_dirs(&dir);
    assert!(!doctor.ok);
    let finding = find(
        &doctor.agents[0].findings,
        Code::DisplacedOriginalLeftBehind,
    );
    assert_eq!(finding.severity, Severity::Error);
    assert!(finding.summary.contains(&left[0].display().to_string()));
}

fn doctor_with_dirs(claude: &Path) -> protocol::IntegrationDoctorResult {
    let codex = temp_dir("cleanup-doctor-codex");
    with_config_dirs(claude, &codex, || {
        doctor_with(
            IntegrationDoctorParams {
                agent: Some(AgentKind::Claude),
            },
            &[],
            &[],
        )
    })
    .expect("doctor")
}

#[test]
fn a_cleanup_whose_directory_sync_fails_is_reported_truthfully() {
    use super::commit::CleanupFault;

    let dir = temp_dir("cleanup-sync");
    install_claude(&dir).expect("seed install");
    let _fault = CleanupFaultGuard::install(|_name| Some(CleanupFault::DirectorySync));

    let paths = install_claude(&dir).expect("a committed install is a success");

    assert_eq!(
        paths.cleanup_incomplete.len(),
        3,
        "{:?}",
        paths.cleanup_incomplete
    );
    assert!(paths
        .cleanup_incomplete
        .iter()
        .all(|entry| entry.contains("removed but not confirmed durable")));
    assert!(
        quarantine_entries(&dir).is_empty(),
        "the originals are gone"
    );
    assert_eq!(
        explicit_status(&dir, AgentKind::Claude).state,
        IntegrationInstallState::Current
    );
}

type UninstallFn = fn(&Path) -> Result<IntegrationUninstallReport, ProtocolError>;

#[test]
fn a_removal_cleanup_failure_is_reported_and_the_registration_edit_stays_committed() {
    use super::commit::CleanupFault;

    for agent in [AgentKind::Claude, AgentKind::Codex] {
        let dir = temp_dir("cleanup-uninstall");
        let (script, uninstall): (PathBuf, UninstallFn) = if agent == AgentKind::Claude {
            install_claude(&dir).expect("install");
            (
                dir.join("hooks").join(STATE_HOOK_INSTALL_NAME),
                uninstall_claude,
            )
        } else {
            install_codex(&dir).expect("install");
            (dir.join(STATE_HOOK_INSTALL_NAME), uninstall_codex)
        };
        let body = fs::read(&script).expect("read script");
        let _fault = CleanupFaultGuard::install(|name| {
            (name == STATE_HOOK_INSTALL_NAME).then_some(CleanupFault::Unlink)
        });

        let report = uninstall(&dir).expect("a committed removal is a success");

        assert_eq!(
            report.state,
            IntegrationUninstallState::Removed,
            "{agent:?}"
        );
        assert_eq!(report.cleanup_incomplete.len(), 1, "{agent:?}");
        assert!(
            !script.exists(),
            "{agent:?}: the script no longer has its name"
        );
        let left = quarantine_entries(script.parent().expect("script parent"));
        assert_eq!(left.len(), 1, "{agent:?}: {left:?}");
        assert_eq!(
            fs::read(&left[0]).expect("quarantined script"),
            body,
            "{agent:?}: no data lost"
        );
        assert!(!read_json(&dir.join(if agent == AgentKind::Claude {
            "settings.json"
        } else {
            "hooks.json"
        }))
        .to_string()
        .contains("pohunek-agent-"));
    }
}

#[test]
fn a_real_unlink_failure_during_cleanup_is_reported_not_raised() {
    if nix::unistd::Uid::effective().is_root() {
        // Root ignores directory permissions, so the failure cannot be provoked.
        return;
    }
    let dir = temp_dir("cleanup-real");
    install_claude(&dir).expect("seed install");
    let hooks = dir.join("hooks");
    let locked = hooks.clone();
    let _hook = RaceHook::install(move |label, _name| {
        if label == "commit.committed" {
            set_mode(&locked, 0o500);
        }
    });

    let outcome = install_claude(&dir);
    set_mode(&hooks, 0o700);

    let paths = outcome.expect("the committed install is a success");
    assert_eq!(
        paths.cleanup_incomplete.len(),
        2,
        "{:?}",
        paths.cleanup_incomplete
    );
    let left = quarantine_entries(&hooks);
    assert_eq!(
        left.len(),
        2,
        "both script originals remain intact: {left:?}"
    );
    assert_eq!(
        explicit_status(&dir, AgentKind::Claude).state,
        IntegrationInstallState::Current
    );
}

/// The four ways a config path can be reached that the trusted walk refuses.
fn refused_homes(base: &Path) -> Vec<(&'static str, PathBuf)> {
    let real = base.join("real");
    fs::create_dir(&real).expect("create real dir");
    set_mode(&real, 0o700);
    fs::create_dir_all(real.join("home").join(".claude")).expect("create real Claude dir");
    fs::create_dir_all(real.join("home").join(".codex")).expect("create real Codex dir");
    set_mode(&real.join("home"), 0o700);
    symlink(&real, base.join("live-link")).expect("live symlink");
    symlink(base.join("nowhere"), base.join("dangling-link")).expect("dangling symlink");
    symlink("/usr", base.join("foreign-link")).expect("symlink to a foreign-owned dir");
    let writable = base.join("writable");
    fs::create_dir_all(writable.join("home").join(".claude")).expect("create writable-parent dirs");
    fs::create_dir_all(writable.join("home").join(".codex")).expect("create writable-parent dirs");
    set_mode(&writable.join("home"), 0o700);
    set_mode(&writable, 0o777);
    vec![
        (
            "live intermediate symlink",
            base.join("live-link").join("home"),
        ),
        (
            "dangling intermediate symlink",
            base.join("dangling-link").join("home"),
        ),
        (
            "symlink to a foreign-owned dir",
            base.join("foreign-link").join("pohunek-home"),
        ),
        ("world-writable parent", writable.join("home")),
    ]
}

#[test]
fn status_and_doctor_refuse_exactly_the_paths_install_and_uninstall_refuse() {
    let base = temp_dir("rm-walk");
    let homes = refused_homes(&base);
    for (name, home) in &homes {
        for agent in [AgentKind::Claude, AgentKind::Codex] {
            let dir_name = if agent == AgentKind::Claude {
                ".claude"
            } else {
                ".codex"
            };
            let config = home.join(dir_name);
            for scope in ["env override", "default"] {
                let context = format!("{name} / {agent:?} / {scope}");
                let run = |operation: &dyn Fn() -> String| -> String {
                    if scope == "env override" {
                        with_config_dirs(&home.join(".claude"), &home.join(".codex"), operation)
                    } else {
                        super::tests::with_status_env(None, None, Some(home), operation)
                    }
                };
                let status = run(&|| {
                    let report = super::status(protocol::IntegrationStatusParams {
                        agent: Some(agent.clone()),
                    })
                    .expect("status")
                    .agents
                    .remove(0);
                    format!(
                        "{:?}|{:?}|{}",
                        report.state, report.recovery, report.available
                    )
                });
                assert_eq!(
                    status,
                    "Outdated|RepairConfiguration|false",
                    "{context}: status must refuse {}",
                    config.display()
                );
                let doctor = run(&|| {
                    let result = doctor_with(
                        IntegrationDoctorParams {
                            agent: Some(agent.clone()),
                        },
                        &[],
                        &[],
                    )
                    .expect("doctor");
                    let codes: Vec<_> = result.agents[0]
                        .findings
                        .iter()
                        .map(|finding| finding.code)
                        .collect();
                    format!("{}|{}", result.ok, codes.contains(&Code::ConfigRootInvalid))
                });
                assert_eq!(doctor, "false|true", "{context}: doctor");
                let install = run(&|| {
                    super::install(Some(agent.clone()))
                        .expect_err("install refuses")
                        .code
                });
                assert_eq!(install, "integration_path_untrusted", "{context}: install");
                let uninstall = run(&|| {
                    super::uninstall(agent.clone())
                        .expect_err("uninstall refuses")
                        .code
                });
                assert_eq!(
                    uninstall, "integration_path_untrusted",
                    "{context}: uninstall"
                );
            }
        }
    }
    // Make the writable parent removable for cleanup by the test harness.
    set_mode(&base.join("writable"), 0o700);
}

#[test]
fn a_swap_after_activation_is_never_deleted_by_the_rollback() {
    for agent in [AgentKind::Claude, AgentKind::Codex] {
        let dir = temp_dir("rm-after-activation");
        let script = if agent == AgentKind::Claude {
            install_claude(&dir).expect("seed install");
            dir.join("hooks").join(STATE_HOOK_INSTALL_NAME)
        } else {
            install_codex(&dir).expect("seed install");
            dir.join(STATE_HOOK_INSTALL_NAME)
        };
        let original = fs::read(&script).expect("read original script");
        let target = script.clone();
        let _hook = RaceHook::install(move |label, name| {
            if label == "write.committed" && name == STATE_HOOK_INSTALL_NAME {
                // Another process replaces the destination right after activation.
                swap_in_unmarked(&target);
            }
        });
        let last = 2;
        let mut gate = move |index: usize, _name: &str| {
            if index == last {
                Err(injected())
            } else {
                Ok(())
            }
        };

        let error = if agent == AgentKind::Claude {
            super::install_claude_gated(&dir, &mut gate).map(|_paths| ())
        } else {
            super::install_codex_gated(&dir, &mut gate).map(|_paths| ())
        }
        .expect_err("the later step fails");

        assert_eq!(error.code, DESTINATION_COLLISION_CODE, "{agent:?}");
        assert_eq!(
            fs::read_to_string(&script).expect("the concurrent file must survive"),
            "#!/bin/sh\necho user-owned\n",
            "{agent:?}: a foreign file was deleted by the rollback"
        );
        assert!(error.msg.contains("the original stays at"), "{}", error.msg);
        let quarantined: Vec<_> = quarantine_entries(script.parent().expect("script parent"));
        let kept = quarantined
            .iter()
            .any(|path| fs::read(path).is_ok_and(|bytes| bytes == original));
        assert!(
            kept,
            "{agent:?}: the original must stay recoverable: {quarantined:?}"
        );
    }
}

#[test]
fn transaction_hints_name_the_operation_that_was_running() {
    // Install: a collision and an unrestorable rollback.
    let dir = temp_dir("hint-install");
    install_claude(&dir).expect("seed install");
    let mut collide = |_index: usize, name: &str| {
        if name == "settings.json" {
            write_json(&dir.join("settings.json"), &json!({ "edited": 1 }));
        }
        Ok(())
    };
    let error = super::install_claude_gated(&dir, &mut collide).expect_err("collision");
    let hint = error.recover.clone().expect("hint");
    assert!(hint.contains("`pohunek integration install`"), "{hint}");
    assert!(!hint.contains("uninstall"), "{hint}");

    let dir = temp_dir("hint-install-recovery");
    install_claude(&dir).expect("seed install");
    let hooks = dir.join("hooks");
    let _vanish = RaceHook::install(move |label, name| {
        if label == "write.committed" && name == STATE_HOOK_INSTALL_NAME {
            for entry in fs::read_dir(&hooks).expect("list").filter_map(Result::ok) {
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
    let mut fail = |_index: usize, name: &str| {
        if name == "settings.json" {
            return Err(injected());
        }
        Ok(())
    };
    let error = super::install_claude_gated(&dir, &mut fail).expect_err("recovery");
    assert_eq!(error.code, RECOVERY_REQUIRED_CODE);
    assert!(
        error.msg.starts_with("integration install failed"),
        "{}",
        error.msg
    );
    let hint = error.recover.expect("hint");
    assert!(
        hint.contains("`pohunek integration install`") && !hint.contains("uninstall"),
        "{hint}"
    );

    // Uninstall: the same two failures must not tell the user to install.
    let dir = temp_dir("hint-uninstall");
    install_claude(&dir).expect("install");
    let mut collide = |_index: usize, name: &str| {
        if name == "settings.json" {
            write_json(&dir.join("settings.json"), &json!({ "edited": 1 }));
        }
        Ok(())
    };
    let error = uninstall_claude_gated(&dir, &mut collide).expect_err("collision");
    assert_eq!(error.code, DESTINATION_COLLISION_CODE);
    let hint = error.recover.expect("hint");
    assert!(hint.contains("`pohunek integration uninstall`"), "{hint}");

    let dir = temp_dir("hint-uninstall-recovery");
    install_claude(&dir).expect("install");
    let state = dir.join("hooks").join(STATE_HOOK_INSTALL_NAME);
    let mut recreate = |_index: usize, name: &str| {
        if name == NOTIFY_HOOK_INSTALL_NAME {
            fs::write(&state, "recreated\n").expect("recreate the removed script");
            return Err(injected());
        }
        Ok(())
    };
    let error = uninstall_claude_gated(&dir, &mut recreate).expect_err("recovery");
    assert_eq!(error.code, RECOVERY_REQUIRED_CODE);
    assert!(
        error.msg.starts_with("integration uninstall failed"),
        "{}",
        error.msg
    );
    let hint = error.recover.expect("hint");
    assert!(hint.contains("`pohunek integration uninstall`"), "{hint}");
}

#[test]
fn the_lock_and_path_hints_are_neutral_between_install_and_uninstall() {
    let dir = temp_dir("hint-lock");
    install_claude(&dir).expect("install");
    let holder = TrustedDir::open(&dir, "test root").expect("open");
    let lock = holder.lock_installer().expect("hold lock");
    let error = uninstall_claude(&dir).expect_err("locked");
    let hint = error.recover.expect("hint");
    assert!(hint.contains("install, uninstall or doctor"), "{hint}");
    drop(lock);

    let file = dir.join("not-a-dir");
    fs::write(&file, "").expect("write file");
    let error = uninstall_claude(&file).expect_err("not a directory");
    assert!(
        !error.recover.expect("hint").contains("reinstalling"),
        "an uninstall must not be told to reinstall"
    );
}

const QUARANTINE_NAME: &str = ".pohunek-integration-displaced-fixture";

#[test]
fn the_quarantine_scan_never_follows_a_symlinked_root_or_hooks_directory() {
    let base = temp_dir("doctor-scan-symlink");
    let outside = base.join("outside");
    fs::create_dir(&outside).expect("create outside dir");
    set_mode(&outside, 0o700);
    fs::write(outside.join(QUARANTINE_NAME), "not ours").expect("plant a look-alike");

    // A symlinked config root.
    let link = base.join("root-link");
    symlink(&outside, &link).expect("symlink root");
    assert!(
        super::doctor::quarantine_findings(&link, &AgentKind::Claude, 100).is_empty(),
        "a symlinked config root must not be scanned"
    );
    let status = explicit_status(&link, AgentKind::Claude);
    assert!(diagnose(status, &[])
        .findings
        .iter()
        .all(|finding| finding.code != Code::DisplacedOriginalLeftBehind));

    // A symlinked hooks directory below a valid root.
    let root = temp_dir("doctor-scan-hooks");
    symlink(&outside, root.join("hooks")).expect("symlink hooks");
    assert!(
        super::doctor::quarantine_findings(&root, &AgentKind::Claude, 100).is_empty(),
        "a symlinked hooks directory must not be scanned"
    );

    // A real hooks directory is scanned, and a real look-alike is reported.
    let real = temp_dir("doctor-scan-real");
    fs::create_dir(real.join("hooks")).expect("create hooks");
    set_mode(&real.join("hooks"), 0o700);
    fs::write(real.join("hooks").join(QUARANTINE_NAME), "left").expect("plant leftover");
    let findings = super::doctor::quarantine_findings(&real, &AgentKind::Claude, 100);
    assert_eq!(findings.len(), 1);
    assert!(findings[0].summary.contains(QUARANTINE_NAME));
}

#[test]
fn the_quarantine_scan_and_report_are_bounded() {
    let dir = temp_dir("doctor-scan-bound");
    for index in 0..12 {
        fs::write(dir.join(format!("{QUARANTINE_NAME}-{index:02}")), "x").expect("plant");
    }
    for index in 0..40 {
        fs::write(dir.join(format!("unrelated-{index}")), "x").expect("plant unrelated");
    }

    // The report lists a bounded number of paths and counts the rest.
    let findings = super::doctor::quarantine_findings(&dir, &AgentKind::Codex, 1000);
    assert_eq!(findings.len(), 1);
    assert!(
        findings[0]
            .summary
            .starts_with("12 quarantined original(s)"),
        "{}",
        findings[0].summary
    );
    assert!(
        findings[0].summary.contains("and 4 more"),
        "{}",
        findings[0].summary
    );
    assert!(!findings[0].summary.contains("scan stopped"));

    // The scan itself stops at the entry bound and says so.
    let crowded = temp_dir("doctor-scan-crowded");
    for index in 0..12 {
        fs::write(crowded.join(format!("{QUARANTINE_NAME}-{index:02}")), "x").expect("plant");
    }
    let cut = super::doctor::quarantine_findings(&crowded, &AgentKind::Codex, 5);
    assert_eq!(cut.len(), 2, "{cut:?}");
    let leftover = find(&cut, Code::DisplacedOriginalLeftBehind);
    assert!(
        leftover.summary.starts_with("5 quarantined original(s)"),
        "{}",
        leftover.summary
    );
    let incomplete = find(&cut, Code::QuarantineScanIncomplete);
    assert!(
        incomplete.summary.contains("stops after 5 entries"),
        "{}",
        incomplete.summary
    );
}

#[test]
fn an_incomplete_quarantine_scan_is_always_an_error_finding() {
    let dir = temp_dir("doctor-scan-incomplete");
    for index in 0..30 {
        fs::write(dir.join(format!("unrelated-{index:02}")), "x").expect("plant unrelated");
    }
    // Readdir order is unspecified, so the look-alike may or may not be reached.
    for with_quarantine_name in [false, true] {
        if with_quarantine_name {
            fs::write(dir.join(format!("{QUARANTINE_NAME}-beyond")), "x").expect("plant");
        }
        let findings = super::doctor::quarantine_findings(&dir, &AgentKind::Codex, 5);
        let incomplete = find(&findings, Code::QuarantineScanIncomplete);
        assert_eq!(
            incomplete.severity,
            Severity::Error,
            "{with_quarantine_name}"
        );
        assert!(incomplete.remediation.is_some());
    }
}

#[test]
fn doctor_is_not_ok_when_the_real_scan_bound_is_exceeded() {
    let codex = temp_dir("doctor-scan-real-bound");
    install_codex(&codex).expect("install");
    for index in 0..4200 {
        fs::write(codex.join(format!("unrelated-{index}")), "").expect("plant unrelated");
    }
    let claude = temp_dir("doctor-scan-real-bound-claude");

    let result = with_config_dirs(&claude, &codex, || {
        doctor_with(
            IntegrationDoctorParams {
                agent: Some(AgentKind::Codex),
            },
            &[],
            &[],
        )
    })
    .expect("doctor");

    assert!(!result.ok, "a truncated scan must not read as healthy");
    find(&result.agents[0].findings, Code::QuarantineScanIncomplete);
    // The fixture holds thousands of entries; do not leave them in the shared temp root.
    fs::remove_dir_all(&codex).expect("remove the crowded fixture");
}

#[test]
fn every_platform_quarantine_prefix_is_scanned() {
    let dir = temp_dir("doctor-scan-platform-prefixes");
    for prefix in pohunek_platform::filesystem::QUARANTINE_NAME_PREFIXES {
        fs::write(dir.join(format!("{prefix}fixture")), "x").expect("plant");
    }
    let findings = super::doctor::quarantine_findings(&dir, &AgentKind::Codex, 1000);
    let leftover = find(&findings, Code::DisplacedOriginalLeftBehind);
    assert!(
        leftover.summary.starts_with(&format!(
            "{} quarantined original(s)",
            pohunek_platform::filesystem::QUARANTINE_NAME_PREFIXES.len()
        )),
        "{}",
        leftover.summary
    );
    assert!(
        leftover.summary.contains(".pohunek-restore-fixture") || leftover.summary.contains("more")
    );
}

#[test]
fn an_unchanged_codex_config_changed_after_load_is_a_collision_and_nothing_is_written() {
    type Foreign = fn(&Path);
    let changes: [(&str, Foreign); 3] = [
        ("content", |config| {
            fs::write(config, "[features]\nhooks = false\n").expect("rewrite config");
        }),
        ("mode only", |config| {
            set_mode(config, 0o640);
        }),
        ("atomic swap", |config| {
            let swap = config.with_extension("swap");
            fs::copy(config, &swap).expect("copy config");
            set_mode(&swap, 0o600);
            fs::rename(&swap, config).expect("swap config");
        }),
    ];
    for (name, change) in changes {
        // The first verification runs before any destructive step, the second
        // right before the registration step.
        for verify_index in [0_usize, 3] {
            let dir = temp_dir("verify-unchanged");
            install_codex(&dir).expect("seed install");
            let config = dir.join("config.toml");
            let before: Vec<_> = content_snapshot(&dir)
                .into_iter()
                .filter(|(path, _mode, _content)| path != Path::new("config.toml"))
                .collect();
            let target = config.clone();
            let mut gate = move |index: usize, _step: &str| {
                if index == verify_index {
                    change(&target);
                }
                Ok(())
            };

            let error = super::install_codex_gated(&dir, &mut gate).expect_err("must collide");

            assert_eq!(
                error.code, DESTINATION_COLLISION_CODE,
                "{name}@{verify_index}"
            );
            let after: Vec<_> = content_snapshot(&dir)
                .into_iter()
                .filter(|(path, _mode, _content)| path != Path::new("config.toml"))
                .collect();
            assert_eq!(
                after, before,
                "{name}@{verify_index}: nothing else is written"
            );
            assert!(quarantine_entries(&dir).is_empty(), "{name}@{verify_index}");
        }
    }
}

#[test]
fn an_unchanged_registration_changed_after_load_blocks_an_uninstall() {
    for agent in [AgentKind::Claude, AgentKind::Codex] {
        for verify_index in [0_usize, 1] {
            let dir = temp_dir("verify-uninstall");
            // A registration file without any managed entry stays unchanged.
            let (registration, uninstall): (PathBuf, UninstallGated) = if agent == AgentKind::Claude
            {
                install_claude(&dir).expect("install");
                write_json(&dir.join("settings.json"), &user_settings());
                (dir.join("settings.json"), uninstall_claude_gated)
            } else {
                install_codex(&dir).expect("install");
                fs::write(dir.join("config.toml"), "model = \"m\"\n").expect("plain config");
                (dir.join("config.toml"), uninstall_codex_gated)
            };
            let scripts_before: Vec<_> = content_snapshot(&dir)
                .into_iter()
                .filter(|(path, _mode, _content)| {
                    path.file_name()
                        .and_then(std::ffi::OsStr::to_str)
                        .is_some_and(|name| name.starts_with("pohunek-agent-"))
                })
                .collect();
            let name = registration
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .expect("file name")
                .to_owned();
            let mut seen = 0_usize;
            let target = registration.clone();
            let mut gate = move |_index: usize, step: &str| {
                if step == name {
                    if seen == verify_index {
                        set_mode(&target, 0o640);
                    }
                    seen += 1;
                }
                Ok(())
            };

            let error = uninstall(&dir, &mut gate).expect_err("must collide");

            assert_eq!(
                error.code, DESTINATION_COLLISION_CODE,
                "{agent:?}@{verify_index}"
            );
            let scripts_after: Vec<_> = content_snapshot(&dir)
                .into_iter()
                .filter(|(path, _mode, _content)| {
                    path.file_name()
                        .and_then(std::ffi::OsStr::to_str)
                        .is_some_and(|name| name.starts_with("pohunek-agent-"))
                })
                .collect();
            assert_eq!(scripts_after, scripts_before, "{agent:?}@{verify_index}");
        }
    }
}

type UninstallGated =
    fn(&Path, super::commit::StepGate<'_>) -> Result<IntegrationUninstallReport, ProtocolError>;

const USER_BEFORE_COMMAND: &str = "echo user-before";
const USER_AFTER_COMMAND: &str = "echo user-after";

fn trust_of(config_dir: &Path, group: usize) -> Option<String> {
    let text = fs::read_to_string(config_dir.join("config.toml")).expect("read config");
    let config: toml::Value = toml::from_str(&text).expect("config stays valid TOML");
    let key = super::codex_hook_trust_key(
        &config_dir.join("hooks.json"),
        super::CODEX_SESSION_START_TRUST_EVENT,
        group,
        0,
    );
    config
        .get("hooks")?
        .get("state")?
        .get(&key)?
        .get("trusted_hash")?
        .as_str()
        .map(str::to_owned)
}

fn managed_state_hash(config_dir: &Path) -> String {
    trust_hash(&super::hook_command(
        &config_dir.join(STATE_HOOK_INSTALL_NAME),
        super::HOOK_ACTION,
    ))
}

/// Appends the user's own `SessionStart` hook after everything in `hooks.json`,
/// approved at its current position in `config.toml`.
fn append_user_hook_with_trust(dir: &Path, command: &str) -> usize {
    let hooks_path = dir.join("hooks.json");
    let mut hooks = read_json(&hooks_path);
    let groups = hooks["hooks"]["SessionStart"]
        .as_array_mut()
        .expect("SessionStart groups");
    groups.push(json!({ "hooks": [{ "type": "command", "command": command }] }));
    let group = groups.len() - 1;
    write_json(&hooks_path, &hooks);
    let mut config = fs::read_to_string(dir.join("config.toml")).expect("read config");
    config.push_str(&trust_table(&hooks_path, group, &trust_hash(command)));
    fs::write(dir.join("config.toml"), config).expect("append user trust");
    group
}

#[test]
fn a_user_hook_after_the_managed_group_keeps_its_own_trust_through_reinstall_and_uninstall() {
    let dir = temp_dir("trust-after");
    install_codex(&dir).expect("install");
    let user_hash = trust_hash(USER_AFTER_COMMAND);
    let group = append_user_hook_with_trust(&dir, USER_AFTER_COMMAND);
    assert_eq!(group, 1, "the user hook follows the managed group");
    assert_eq!(
        explicit_status(&dir, AgentKind::Codex).state,
        IntegrationInstallState::Current
    );

    // Reinstall removes the managed group and appends it again: the user hook
    // moves to position 0 and its record must move with it.
    install_codex(&dir).expect("reinstall");
    assert_eq!(
        trust_of(&dir, 0).as_deref(),
        Some(user_hash.as_str()),
        "user record follows its hook"
    );
    assert_eq!(
        trust_of(&dir, 1),
        Some(managed_state_hash(&dir)),
        "the managed record is not overwriting the user's"
    );
    assert_eq!(
        explicit_status(&dir, AgentKind::Codex).state,
        IntegrationInstallState::Current
    );
    install_codex(&dir).expect("a second reinstall is stable");
    assert_eq!(trust_of(&dir, 0).as_deref(), Some(user_hash.as_str()));

    // Uninstall from a [managed, user] layout: the user hook moves to 0 again.
    let mut hooks = read_json(&dir.join("hooks.json"));
    let groups = hooks["hooks"]["SessionStart"]
        .as_array_mut()
        .expect("groups");
    groups.swap(0, 1);
    write_json(&dir.join("hooks.json"), &hooks);
    let config = fs::read_to_string(dir.join("config.toml")).expect("read config");
    let swapped = config
        .replace(&user_hash, "PLACEHOLDER")
        .replace(&managed_state_hash(&dir), &user_hash)
        .replace("PLACEHOLDER", &managed_state_hash(&dir));
    fs::write(dir.join("config.toml"), swapped).expect("swap trust records with their hooks");
    assert_eq!(trust_of(&dir, 0), Some(managed_state_hash(&dir)));
    assert_eq!(trust_of(&dir, 1).as_deref(), Some(user_hash.as_str()));

    uninstall_codex(&dir).expect("uninstall");

    assert_eq!(
        trust_of(&dir, 0).as_deref(),
        Some(user_hash.as_str()),
        "the user's record follows its hook to position 0"
    );
    assert_eq!(
        trust_of(&dir, 1),
        None,
        "no record is left under the old index"
    );
    assert!(read_json(&dir.join("hooks.json"))
        .to_string()
        .contains(USER_AFTER_COMMAND));
}

#[test]
fn a_user_hook_before_the_managed_group_keeps_its_trust_through_every_operation() {
    let dir = temp_dir("trust-before");
    let hooks_path = dir.join("hooks.json");
    write_json(
        &hooks_path,
        &json!({ "hooks": { "SessionStart": [
            { "hooks": [{ "type": "command", "command": USER_BEFORE_COMMAND }] }
        ]}}),
    );
    let user_hash = trust_hash(USER_BEFORE_COMMAND);
    fs::write(
        dir.join("config.toml"),
        trust_table(&hooks_path, 0, &user_hash),
    )
    .expect("seed user trust");

    install_codex(&dir).expect("install");
    assert_eq!(trust_of(&dir, 0).as_deref(), Some(user_hash.as_str()));
    assert_eq!(trust_of(&dir, 1), Some(managed_state_hash(&dir)));
    assert_eq!(
        explicit_status(&dir, AgentKind::Codex).state,
        IntegrationInstallState::Current
    );
    install_codex(&dir).expect("reinstall");
    assert_eq!(trust_of(&dir, 0).as_deref(), Some(user_hash.as_str()));
    assert_eq!(trust_of(&dir, 1), Some(managed_state_hash(&dir)));

    uninstall_codex(&dir).expect("uninstall");
    assert_eq!(trust_of(&dir, 0).as_deref(), Some(user_hash.as_str()));
    assert_eq!(trust_of(&dir, 1), None);
}

#[test]
fn two_claims_on_one_trust_key_fail_closed_without_writing() {
    // [managed, user] with the user's record at 1 and a foreign record where the
    // user's hook must move to: neither claim may be chosen.
    for operation in ["install", "uninstall"] {
        let dir = temp_dir("trust-conflict");
        install_codex(&dir).expect("install");
        append_user_hook_with_trust(&dir, USER_AFTER_COMMAND);
        let config = fs::read_to_string(dir.join("config.toml")).expect("read config");
        let foreign = config.replace(&managed_state_hash(&dir), "sha256:some-other-claim");
        fs::write(dir.join("config.toml"), foreign).expect("plant the foreign claim at key 0");
        let before = content_snapshot(&dir);

        let error = if operation == "install" {
            install_codex(&dir).map(|_paths| ())
        } else {
            uninstall_codex(&dir).map(|_report| ())
        }
        .expect_err("a colliding claim must fail closed");

        assert_eq!(error.code, "integration_trust_conflict", "{operation}");
        assert_eq!(
            content_snapshot(&dir),
            before,
            "{operation}: nothing is written"
        );
        assert!(quarantine_entries(&dir).is_empty(), "{operation}");
    }
}

/// Runs `command -v python3` in `sh` with exactly `dirs` as `PATH`.
fn shell_finds_python3(dirs: &[PathBuf]) -> bool {
    let path = std::env::join_paths(dirs).expect("join PATH");
    // Absolute path: the child's `PATH` is exactly the layout under test.
    let output = std::process::Command::new("/bin/sh")
        .args(["-c", "command -v python3"])
        .env_clear()
        .env("PATH", path)
        .output()
        .expect("run sh");
    output.status.success() && !output.stdout.is_empty()
}

fn plain_probe(dirs: Vec<PathBuf>) -> PythonProbe {
    PythonProbe {
        search_dirs: dirs,
        macos_stub: None,
    }
}

#[test]
fn python_probe_uses_the_effective_execute_permission_like_the_shell() {
    let root = temp_dir("doctor-python-access");
    let marker = root.join("was-executed");
    let usable = root.join("usable");
    write_probe_script(&usable, &marker);

    // Owner has no execute bit while the group has one: the owner class decides,
    // so this process cannot run it although an execute bit is set.
    let group_only = root.join("group-only");
    let candidate = write_probe_script(&group_only, &marker);
    set_mode(&candidate, 0o010);
    // A directory and a plain data file named python3.
    let directory = root.join("directory");
    fs::create_dir_all(directory.join("python3")).expect("directory named python3");
    let data = root.join("data");
    fs::create_dir(&data).expect("data dir");
    fs::write(data.join("python3"), "x").expect("data file");
    set_mode(&data.join("python3"), 0o644);

    let layouts: [(&str, Vec<PathBuf>); 6] = [
        (
            "group-only first, usable later",
            vec![group_only.clone(), usable.clone()],
        ),
        ("group-only alone", vec![group_only.clone()]),
        (
            "directory first, usable later",
            vec![directory.clone(), usable.clone()],
        ),
        (
            "data file first, usable later",
            vec![data.clone(), usable.clone()],
        ),
        ("usable first", vec![usable.clone(), group_only.clone()]),
        ("nothing usable", vec![data.clone(), directory.clone()]),
    ];
    for (name, dirs) in layouts {
        let expected = shell_finds_python3(&dirs);
        let state = plain_probe(dirs).state();
        assert_eq!(
            matches!(state, PythonState::Found(_)),
            expected,
            "{name}: the probe must agree with the shell's own PATH search ({state:?})"
        );
        if !expected {
            assert_eq!(state, PythonState::Missing, "{name}");
        }
    }
    let findings = python_findings(&plain_probe(vec![group_only]));
    assert_eq!(findings[0].severity, Severity::Info);
    assert_eq!(
        findings[0].code == Code::HookRuntimePythonFound,
        nix::unistd::Uid::effective().is_root(),
        "an unrunnable python3 alone is only a note"
    );
    assert!(!marker.exists(), "the probe must never execute python3");
}

/// How another writer changes a registration file the transaction just wrote.
#[derive(Clone, Copy, Debug)]
enum ConcurrentChange {
    /// Puts the pre-operation content back through an atomic swap.
    RestoreBySwap,
    /// Writes the pre-operation content into the same inode.
    RestoreInPlace,
    /// Changes only the permissions.
    ChmodOnly,
}

fn apply_concurrent_change(change: ConcurrentChange, path: &Path, backup: &[u8]) {
    match change {
        ConcurrentChange::RestoreBySwap => {
            let swap = path.with_extension("swap");
            fs::write(&swap, backup).expect("write the restored content");
            set_mode(&swap, 0o600);
            fs::rename(&swap, path).expect("swap the restored file in");
        }
        ConcurrentChange::RestoreInPlace => fs::write(path, backup).expect("restore in place"),
        ConcurrentChange::ChmodOnly => set_mode(path, 0o640),
    }
}

const CONCURRENT_CHANGES: [ConcurrentChange; 3] = [
    ConcurrentChange::RestoreBySwap,
    ConcurrentChange::RestoreInPlace,
    ConcurrentChange::ChmodOnly,
];

fn script_snapshot(dir: &Path) -> Vec<(PathBuf, u32, Vec<u8>)> {
    content_snapshot(dir)
        .into_iter()
        .filter(|(path, _mode, _content)| {
            path.file_name()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|name| name.starts_with("pohunek-agent-"))
        })
        .collect()
}

#[test]
fn a_registration_changed_after_it_was_written_blocks_the_script_removal() {
    for (agent, registrations) in [
        (AgentKind::Claude, vec!["settings.json"]),
        (AgentKind::Codex, vec!["hooks.json", "config.toml"]),
    ] {
        for registration in registrations {
            for change in CONCURRENT_CHANGES {
                let dir = temp_dir("rewrite-check-removal");
                if agent == AgentKind::Claude {
                    write_json(&dir.join("settings.json"), &user_settings());
                    install_claude(&dir).expect("install");
                } else {
                    fs::write(dir.join("config.toml"), "model = \"m\"\n").expect("seed config");
                    install_codex(&dir).expect("install");
                }
                let path = dir.join(registration);
                let backup = fs::read(&path).expect("read the pre-removal registration");
                let scripts_before = script_snapshot(&dir);
                let target = path.clone();
                let saved = backup.clone();
                let _hook = RaceHook::install(move |label, _name| {
                    if label == "commit.before_removal" {
                        apply_concurrent_change(change, &target, &saved);
                    }
                });

                let error = if agent == AgentKind::Claude {
                    uninstall_claude(&dir).map(|_report| ())
                } else {
                    uninstall_codex(&dir).map(|_report| ())
                }
                .expect_err("the changed registration must abort the removal");

                let context = format!("{agent:?}/{registration}/{change:?}");
                assert_eq!(error.code, DESTINATION_COLLISION_CODE, "{context}");
                assert_eq!(
                    script_snapshot(&dir),
                    scripts_before,
                    "{context}: scripts untouched"
                );
                assert!(
                    error.msg.contains("the original stays at"),
                    "{context}: {}",
                    error.msg
                );
                match change {
                    ConcurrentChange::ChmodOnly => {
                        let mode =
                            fs::metadata(&path).expect("metadata").permissions().mode() & 0o7777;
                        assert_eq!(
                            mode, 0o640,
                            "{context}: the concurrent permissions are kept"
                        );
                    }
                    _ => assert_eq!(
                        fs::read(&path).expect("read"),
                        backup,
                        "{context}: the concurrent version is kept"
                    ),
                }
            }
        }
    }
}

#[test]
fn a_registration_changed_just_before_the_end_restores_the_removed_scripts() {
    for agent in [AgentKind::Claude, AgentKind::Codex] {
        let dir = temp_dir("rewrite-check-finalize");
        let registration = if agent == AgentKind::Claude {
            install_claude(&dir).expect("install");
            "settings.json"
        } else {
            install_codex(&dir).expect("install");
            "hooks.json"
        };
        let path = dir.join(registration);
        let backup = fs::read(&path).expect("read registration");
        let scripts_before = script_snapshot(&dir);
        let target = path.clone();
        let saved = backup.clone();
        let _hook = RaceHook::install(move |label, _name| {
            if label == "commit.before_finalize" {
                apply_concurrent_change(ConcurrentChange::RestoreBySwap, &target, &saved);
            }
        });

        let error = if agent == AgentKind::Claude {
            uninstall_claude(&dir).map(|_report| ())
        } else {
            uninstall_codex(&dir).map(|_report| ())
        }
        .expect_err("the changed registration must abort the removal");

        assert_eq!(error.code, DESTINATION_COLLISION_CODE, "{agent:?}");
        assert_eq!(
            script_snapshot(&dir),
            scripts_before,
            "{agent:?}: the removed scripts are moved back"
        );
        assert_eq!(
            fs::read(&path).expect("read"),
            backup,
            "{agent:?}: their version is kept"
        );
    }
}

#[test]
fn a_hooks_file_changed_after_it_was_written_blocks_the_install() {
    for change in CONCURRENT_CHANGES {
        for (label, config_changes) in
            [("write.committed", true), ("commit.before_finalize", false)]
        {
            let dir = temp_dir("rewrite-check-install");
            if config_changes {
                install_codex(&dir).expect("seed install");
                fs::write(dir.join("config.toml"), "model = \"other\"\n").expect("age config");
            } else {
                install_codex(&dir).expect("seed install");
            }
            let hooks = dir.join("hooks.json");
            // Differs from what the install writes, so an in-place write is a change.
            let backup = b"{\"hooks\": {}, \"concurrent\": true}".to_vec();
            let scripts_before = script_snapshot(&dir);
            let target = hooks.clone();
            let saved = backup.clone();
            let _hook = RaceHook::install(move |point, name| {
                let hit = if point == "write.committed" {
                    name == "hooks.json"
                } else {
                    point == label
                };
                if point == label && hit {
                    apply_concurrent_change(change, &target, &saved);
                }
            });

            let error = install_codex(&dir).expect_err("the changed hooks.json must abort");

            let context = format!("{change:?}/{label}");
            assert_eq!(error.code, DESTINATION_COLLISION_CODE, "{context}");
            assert_eq!(
                script_snapshot(&dir),
                scripts_before,
                "{context}: scripts rolled back exactly"
            );
            match change {
                ConcurrentChange::ChmodOnly => {
                    let mode =
                        fs::metadata(&hooks).expect("metadata").permissions().mode() & 0o7777;
                    assert_eq!(mode, 0o640, "{context}");
                }
                _ => assert_eq!(fs::read(&hooks).expect("read"), backup, "{context}"),
            }
        }
    }
}

const CONCURRENT_SCRIPT: &str = "#!/bin/sh\n# edited by someone else\n";

/// Runs `operation` with a race hook that edits `target` in place right after
/// the step named `after_step` wrote it, and a gate that fails the step named
/// `fail_at`.
fn edit_after_write_then_fail(
    target: &Path,
    after_step: &'static str,
    fail_at: &'static str,
    body: &'static str,
    operation: impl FnOnce(
        &mut dyn FnMut(usize, &str) -> Result<(), ProtocolError>,
    ) -> Result<(), ProtocolError>,
) -> ProtocolError {
    let edited = target.to_path_buf();
    let _hook = RaceHook::install(move |label, name| {
        if label == "write.committed" && name == after_step {
            fs::write(&edited, body).expect("edit the written file in place");
        }
    });
    let mut gate = move |_index: usize, name: &str| {
        if name == fail_at {
            Err(injected())
        } else {
            Ok(())
        }
    };
    operation(&mut gate).expect_err("the later step fails")
}

#[test]
fn an_install_rollback_keeps_a_file_edited_in_place_after_it_was_written() {
    // Install: a script is edited after it was written, then a later step fails.
    for agent in [AgentKind::Claude, AgentKind::Codex] {
        let dir = temp_dir("rollback-verify-install");
        let (script, other, fail_at): (PathBuf, PathBuf, &'static str) =
            if agent == AgentKind::Claude {
                install_claude(&dir).expect("seed");
                (
                    dir.join("hooks").join(STATE_HOOK_INSTALL_NAME),
                    dir.join("hooks").join(NOTIFY_HOOK_INSTALL_NAME),
                    "settings.json",
                )
            } else {
                install_codex(&dir).expect("seed");
                (
                    dir.join(STATE_HOOK_INSTALL_NAME),
                    dir.join(NOTIFY_HOOK_INSTALL_NAME),
                    "hooks.json",
                )
            };
        let other_before = fs::read(&other).expect("read the other script");

        let error = edit_after_write_then_fail(
            &script,
            STATE_HOOK_INSTALL_NAME,
            fail_at,
            CONCURRENT_SCRIPT,
            |gate| {
                if agent == AgentKind::Claude {
                    super::install_claude_gated(&dir, gate).map(|_paths| ())
                } else {
                    super::install_codex_gated(&dir, gate).map(|_paths| ())
                }
            },
        );

        assert_eq!(error.code, DESTINATION_COLLISION_CODE, "{agent:?} install");
        assert!(error.msg.contains("injected_step_failure"), "{}", error.msg);
        assert_eq!(
            fs::read_to_string(&script).expect("read"),
            CONCURRENT_SCRIPT,
            "{agent:?} install: the concurrent edit must survive the rollback"
        );
        assert_eq!(
            fs::read(&other).expect("read"),
            other_before,
            "{agent:?}: the rest is restored"
        );
    }
}

#[test]
fn an_uninstall_rollback_keeps_a_file_edited_in_place_after_it_was_written() {
    // Uninstall: the rewritten registration is edited, then a later removal fails.
    for agent in [AgentKind::Claude, AgentKind::Codex] {
        let dir = temp_dir("rollback-verify-uninstall");
        let (registration, registration_name, scripts): (PathBuf, &'static str, Vec<PathBuf>) =
            if agent == AgentKind::Claude {
                install_claude(&dir).expect("install");
                (
                    dir.join("settings.json"),
                    "settings.json",
                    vec![
                        dir.join("hooks").join(STATE_HOOK_INSTALL_NAME),
                        dir.join("hooks").join(NOTIFY_HOOK_INSTALL_NAME),
                    ],
                )
            } else {
                install_codex(&dir).expect("install");
                (
                    dir.join("hooks.json"),
                    "hooks.json",
                    vec![
                        dir.join(STATE_HOOK_INSTALL_NAME),
                        dir.join(NOTIFY_HOOK_INSTALL_NAME),
                    ],
                )
            };
        let scripts_before: Vec<_> = scripts
            .iter()
            .map(|p| fs::read(p).expect("read script"))
            .collect();

        let error = edit_after_write_then_fail(
            &registration,
            registration_name,
            NOTIFY_HOOK_INSTALL_NAME,
            "{\"edited\":\"by someone else\"}",
            |gate| {
                if agent == AgentKind::Claude {
                    uninstall_claude_gated(&dir, gate).map(|_report| ())
                } else {
                    uninstall_codex_gated(&dir, gate).map(|_report| ())
                }
            },
        );

        assert_eq!(
            error.code, DESTINATION_COLLISION_CODE,
            "{agent:?} uninstall"
        );
        assert_eq!(
            fs::read_to_string(&registration).expect("read"),
            "{\"edited\":\"by someone else\"}",
            "{agent:?} uninstall: the concurrent edit must survive the rollback"
        );
        for (script, before) in scripts.iter().zip(scripts_before) {
            assert_eq!(
                fs::read(script).expect("script restored"),
                before,
                "{agent:?}"
            );
        }
    }
}

#[test]
fn doctor_does_not_conclude_anything_while_an_operation_holds_the_lock() {
    let claude = temp_dir("doctor-busy");
    install_claude(&claude).expect("install");
    fs::write(
        claude.join(".pohunek-remove-in-flight"),
        "parked by a running operation",
    )
    .expect("plant an in-flight parked file");
    let codex = temp_dir("doctor-busy-codex");
    let params = IntegrationDoctorParams {
        agent: Some(AgentKind::Claude),
    };

    let holder = TrustedDir::open(&claude, "test root").expect("open");
    let lock = holder.lock_installer().expect("an operation is running");
    let busy = with_config_dirs(&claude, &codex, || doctor_with(params.clone(), &[], &[]))
        .expect("doctor");
    assert!(busy.ok, "a running operation is not a failure");
    assert!(
        busy.agents[0].status.is_none(),
        "nothing is read while another operation runs"
    );
    assert_eq!(
        busy.agents[0].findings.len(),
        1,
        "{:?}",
        busy.agents[0].findings
    );
    let finding = &busy.agents[0].findings[0];
    assert_eq!(finding.code, Code::OperationInProgress);
    assert_eq!(finding.severity, Severity::Info);
    assert!(finding.remediation.is_some());
    drop(lock);

    let idle = with_config_dirs(&claude, &codex, || doctor_with(params, &[], &[])).expect("doctor");
    find(&idle.agents[0].findings, Code::DisplacedOriginalLeftBehind);
    assert!(idle.agents[0]
        .findings
        .iter()
        .all(|finding| finding.code != Code::OperationInProgress));
}

#[test]
fn the_doctor_lock_probe_never_creates_the_lock_file() {
    let dir = temp_dir("doctor-no-lock-file");
    assert!(!dir.join(INSTALL_LOCK_NAME).exists());
    let other = temp_dir("doctor-no-lock-file-codex");

    let result = with_config_dirs(&dir, &other, || {
        doctor_with(
            IntegrationDoctorParams {
                agent: Some(AgentKind::Claude),
            },
            &[],
            &[],
        )
    })
    .expect("doctor");

    assert!(result.ok);
    assert!(
        !dir.join(INSTALL_LOCK_NAME).exists(),
        "a read-only doctor creates no file"
    );
}

#[test]
fn a_symlink_or_hard_link_alias_of_the_macos_stub_is_still_the_stub() {
    let root = temp_dir("doctor-stub-alias");
    let marker = root.join("was-executed");
    let stub_dir = root.join("usr-bin");
    let stub = write_probe_script(&stub_dir, &marker);
    let developer = root.join("clt").join("python3");
    let probe = |dirs: Vec<PathBuf>| PythonProbe {
        search_dirs: dirs,
        macos_stub: Some(MacosStub {
            stub: stub.clone(),
            developer_pythons: vec![developer.clone()],
        }),
    };

    let symlink_dir = root.join("alias-link");
    fs::create_dir(&symlink_dir).expect("create alias dir");
    symlink(&stub, symlink_dir.join("python3")).expect("symlink alias");
    let hardlink_dir = root.join("alias-hard");
    fs::create_dir(&hardlink_dir).expect("create hard link dir");
    fs::hard_link(&stub, hardlink_dir.join("python3")).expect("hard link alias");
    let unnormalized_dir = stub_dir.join("..").join("usr-bin");

    for (name, dir) in [
        ("symlink", symlink_dir),
        ("hard link", hardlink_dir),
        ("unnormalized path", unnormalized_dir),
    ] {
        assert_eq!(
            python_findings(&probe(vec![dir]))
                .first()
                .map(|finding| finding.code),
            Some(Code::HookRuntimeMacosShim),
            "{name} alias of the stub must still be the stub"
        );
    }
    assert!(!marker.exists(), "the probe must never execute python3");
}

#[test]
fn an_unsafe_installer_lock_is_an_error_finding() {
    type Break = fn(&Path);
    let breaks: [(&str, Break); 3] = [
        ("symlink", |lock| {
            let target = lock.with_extension("target");
            fs::write(&target, "").expect("write target");
            fs::remove_file(lock).expect("remove lock");
            symlink(&target, lock).expect("replace the lock with a symlink");
        }),
        ("directory", |lock| {
            fs::remove_file(lock).expect("remove lock");
            fs::create_dir(lock).expect("replace the lock with a directory");
        }),
        ("wrong mode", |lock| set_mode(lock, 0o644)),
    ];
    for (name, break_lock) in breaks {
        let claude = temp_dir("doctor-unsafe-lock");
        install_claude(&claude).expect("install");
        break_lock(&claude.join(INSTALL_LOCK_NAME));
        let codex = temp_dir("doctor-unsafe-lock-codex");

        let result = with_config_dirs(&claude, &codex, || {
            doctor_with(
                IntegrationDoctorParams {
                    agent: Some(AgentKind::Claude),
                },
                &[],
                &[],
            )
        })
        .expect("doctor");

        assert!(!result.ok, "{name}: an unusable lock blocks every install");
        let finding = find(&result.agents[0].findings, Code::UnsafeInstallerLock);
        assert_eq!(finding.severity, Severity::Error, "{name}");
        assert!(finding.remediation.is_some(), "{name}");
        assert!(
            result.agents[0].status.is_some(),
            "{name}: the rest of the diagnosis still runs"
        );
    }
}

#[test]
fn the_doctor_holds_the_lock_for_its_whole_inspection() {
    let claude = temp_dir("doctor-holds-lock");
    install_claude(&claude).expect("install");
    let codex = temp_dir("doctor-holds-lock-codex");
    let seen = std::rc::Rc::new(std::cell::RefCell::new(None::<ProtocolError>));
    let sink = std::rc::Rc::clone(&seen);
    let target = claude.clone();
    let _hook = RaceHook::install(move |label, _name| {
        if label == "doctor.inspecting" {
            *sink.borrow_mut() = install_claude(&target).err();
        }
    });

    let result = with_config_dirs(&claude, &codex, || {
        doctor_with(
            IntegrationDoctorParams {
                agent: Some(AgentKind::Claude),
            },
            &[],
            &[],
        )
    })
    .expect("doctor");

    assert!(result.agents[0].status.is_some());
    let error = seen
        .borrow()
        .clone()
        .expect("an installer arriving mid-inspection fails fast");
    assert_eq!(error.code, INSTALL_IN_PROGRESS_CODE);
    assert!(
        error.msg.contains("install, uninstall or doctor"),
        "{}",
        error.msg
    );
    // The guard is released afterwards.
    install_claude(&claude).expect("the lock is free again");
}

#[test]
fn an_operation_that_creates_the_lock_during_the_inspection_drops_the_conclusions() {
    let claude = temp_dir("doctor-lock-appears");
    let codex = temp_dir("doctor-lock-appears-codex");
    assert!(!claude.join(INSTALL_LOCK_NAME).exists());
    let target = claude.clone();
    let _hook = RaceHook::install(move |label, _name| {
        if label == "doctor.inspecting" {
            install_claude(&target).expect("a whole install runs during the inspection");
        }
    });

    let result = with_config_dirs(&claude, &codex, || {
        doctor_with(
            IntegrationDoctorParams {
                agent: Some(AgentKind::Claude),
            },
            &[],
            &[],
        )
    })
    .expect("doctor");

    assert!(result.ok);
    assert!(result.agents[0].status.is_none());
    assert_eq!(result.agents[0].findings[0].code, Code::OperationInProgress);
}

#[test]
fn a_failed_created_directory_restore_is_recovery_required_with_the_true_path() {
    let dir = temp_dir("created-dir-recovery");
    let target = dir.clone();
    let _hook = RaceHook::install(move |label, name| {
        if label == "created_dir.staged" && name == "hooks" {
            // Files appear inside the moved-aside directory and the provider
            // recreates `hooks/`, so it cannot be moved back.
            let quarantined = fs::read_dir(&target)
                .expect("list")
                .filter_map(Result::ok)
                .find(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.starts_with(".pohunek-integration-created-"))
                })
                .expect("the created directory is quarantined");
            fs::write(quarantined.path().join("user.txt"), "kept").expect("concurrent file");
            fs::create_dir(target.join("hooks")).expect("the provider recreates hooks/");
        }
    });
    let mut gate = |_index: usize, name: &str| {
        if name == "settings.json" {
            Err(injected())
        } else {
            Ok(())
        }
    };

    let error = super::install_claude_gated(&dir, &mut gate).expect_err("install fails");

    assert_eq!(error.code, RECOVERY_REQUIRED_CODE);
    assert!(error.msg.contains("injected_step_failure"), "{}", error.msg);
    let quarantine = quarantine_entries(&dir)
        .into_iter()
        .find(|path| {
            path.file_name()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|name| name.starts_with(".pohunek-integration-created-"))
        })
        .expect("the directory stays quarantined");
    assert!(
        error.msg.contains(&quarantine.display().to_string()),
        "{}",
        error.msg
    );
    assert_eq!(
        fs::read_to_string(quarantine.join("user.txt")).expect("the concurrent file is kept"),
        "kept"
    );
}

/// The single quarantine entry in `dir` whose name starts with `prefix`.
fn quarantine_named(dir: &Path, prefix: &str) -> PathBuf {
    fs::read_dir(dir)
        .expect("list")
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(prefix))
        })
        .unwrap_or_else(|| panic!("no {prefix}* entry in {}", dir.display()))
        .path()
}

#[test]
fn a_written_file_changed_between_the_check_and_the_removal_is_kept_and_reported() {
    let dir = temp_dir("rollback-race-remove");
    let hooks = dir.join("hooks");
    let scan = hooks.clone();
    let _hook = RaceHook::install(move |label, name| {
        if label == "rollback.verified" && name == STATE_HOOK_INSTALL_NAME {
            // The moved-aside file is edited after its content was verified.
            let quarantined = quarantine_named(&scan, ".pohunek-integration-rollback-");
            fs::write(&quarantined, "changed after the check").expect("edit in place");
        }
    });
    let mut gate = |_index: usize, name: &str| {
        if name == "settings.json" {
            Err(injected())
        } else {
            Ok(())
        }
    };

    let error = super::install_claude_gated(&dir, &mut gate).expect_err("install fails");

    assert_eq!(error.code, RECOVERY_REQUIRED_CODE);
    let kept = quarantine_named(&hooks, ".pohunek-integration-rollback-");
    assert!(
        error.msg.contains(&kept.display().to_string()),
        "{}",
        error.msg
    );
    assert_eq!(
        fs::read_to_string(&kept).expect("kept"),
        "changed after the check"
    );
}

#[test]
fn a_created_directory_changed_after_it_looked_empty_is_kept_and_reported() {
    let dir = temp_dir("created-dir-changed");
    let target = dir.clone();
    let _hook = RaceHook::install(move |label, name| {
        if label == "created_dir.checked" && name == "hooks" {
            // The quarantined directory is swapped for another one. The
            // original is moved aside first, so the replacement is guaranteed
            // a different inode and no timestamp granularity is involved.
            let quarantined = quarantine_named(&target, ".pohunek-integration-created-");
            fs::rename(&quarantined, target.join("moved-aside-original")).expect("move aside");
            fs::create_dir(&quarantined).expect("another directory takes the name");
            fs::write(quarantined.join("late.txt"), "kept").expect("a file in the replacement");
        }
    });
    let mut gate = |_index: usize, name: &str| {
        if name == "settings.json" {
            Err(injected())
        } else {
            Ok(())
        }
    };

    let error = super::install_claude_gated(&dir, &mut gate).expect_err("install fails");

    assert_eq!(error.code, RECOVERY_REQUIRED_CODE);
    let quarantined = quarantine_named(&dir, ".pohunek-integration-created-");
    assert!(
        error.msg.contains(&quarantined.display().to_string()),
        "{}",
        error.msg
    );
    assert_eq!(
        fs::read_to_string(quarantined.join("late.txt")).expect("kept"),
        "kept"
    );
}

#[test]
fn a_staging_failure_that_names_a_location_is_reported_and_one_that_does_not_is_not() {
    let stays = PathBuf::from("/agent/.pohunek-integration-created-abc");
    let located = pohunek_platform::filesystem::FsError::StagedRecoveryRequired {
        path: stays.clone(),
        source: Box::new(pohunek_platform::filesystem::FsError::IdentityChanged {
            path: stays.clone(),
        }),
    };
    assert_eq!(super::created_dir_stage_failure(&located), Err(stays));

    let unlocated = pohunek_platform::filesystem::FsError::IdentityChanged {
        path: PathBuf::from("/agent/hooks"),
    };
    assert_eq!(super::created_dir_stage_failure(&unlocated), Ok(()));
}

#[test]
fn unsafe_paths_in_a_not_installed_config_still_fail_the_doctor_and_the_install() {
    type Prepare = fn(&Path);
    let claude_cases: [(&str, Prepare); 3] = [
        ("symlinked hooks directory", |dir| {
            let elsewhere = dir.with_extension("elsewhere");
            fs::create_dir(&elsewhere).expect("create target");
            set_mode(&elsewhere, 0o700);
            symlink(&elsewhere, dir.join("hooks")).expect("symlink hooks");
        }),
        ("group-writable hooks directory", |dir| {
            fs::create_dir(dir.join("hooks")).expect("create hooks");
            set_mode(&dir.join("hooks"), 0o775);
        }),
        ("group-writable config directory", |dir| {
            set_mode(dir, 0o775);
        }),
    ];
    for (name, prepare) in claude_cases {
        let claude = temp_dir("not-installed-unsafe-claude");
        prepare(&claude);
        let codex = temp_dir("not-installed-unsafe-claude-other");

        let result = with_config_dirs(&claude, &codex, || {
            doctor_with(
                IntegrationDoctorParams {
                    agent: Some(AgentKind::Claude),
                },
                &[],
                &[],
            )
        })
        .expect("doctor");

        assert!(!result.ok, "{name}: an unsafe path must fail the doctor");
        assert!(
            result.agents[0]
                .findings
                .iter()
                .any(|finding| finding.severity == Severity::Error),
            "{name}: {:?}",
            result.agents[0].findings
        );
        assert_eq!(
            install_claude(&claude)
                .expect_err("install refuses the same path")
                .code,
            "integration_path_untrusted",
            "{name}"
        );
    }

    let codex = temp_dir("not-installed-unsafe-codex");
    set_mode(&codex, 0o775);
    let other = temp_dir("not-installed-unsafe-codex-other");
    let result = with_config_dirs(&other, &codex, || {
        doctor_with(
            IntegrationDoctorParams {
                agent: Some(AgentKind::Codex),
            },
            &[],
            &[],
        )
    })
    .expect("doctor");
    assert!(
        !result.ok,
        "a group-writable Codex config directory fails the doctor"
    );
    assert_eq!(
        install_codex(&codex).expect_err("install refuses").code,
        "integration_path_untrusted"
    );
}

#[test]
fn an_empty_safe_config_is_still_only_informational() {
    for agent in [AgentKind::Claude, AgentKind::Codex] {
        let dir = temp_dir("not-installed-safe");
        let other = temp_dir("not-installed-safe-other");
        let result = with_config_dirs(
            if agent == AgentKind::Claude {
                &dir
            } else {
                &other
            },
            if agent == AgentKind::Codex {
                &dir
            } else {
                &other
            },
            || {
                doctor_with(
                    IntegrationDoctorParams {
                        agent: Some(agent.clone()),
                    },
                    &[],
                    &[],
                )
            },
        )
        .expect("doctor");
        assert!(result.ok, "{agent:?}");
        assert_eq!(
            result.agents[0].findings.len(),
            1,
            "{agent:?}: {:?}",
            result.agents[0].findings
        );
        assert_eq!(result.agents[0].findings[0].code, Code::HooksNotInstalled);
    }
}
