//! Install, status, doctor and uninstall of the Cline hook files across
//! config homes, driven through the same entry points the CLI reaches.

// Rust guideline compliant 2026-11-06

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt as _};
use std::path::{Path, PathBuf};

use pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST;
use protocol::{
    IntegrationDoctorParams, IntegrationInstallResult, IntegrationInstallState,
    IntegrationRecovery, IntegrationStatusParams, IntegrationUninstallState, ProtocolError,
    RuntimeRef,
};

use super::doctor::doctor_in_with;
use super::handler::{install_in, install_in_gated, status_in, RetainedSchemas};
use super::tests::{scoped_dir, tree_snapshot, TestDir};
use super::{uninstall_in, ConfigHomes, HomeSelection};
use crate::agent::host::fixture::{
    cline_shaped_definition, cline_shaped_document, cline_shaped_host, CLINE_REQUIRES,
    PACKAGE_DIGEST,
};
use crate::agent::ProfileRegistry;
use crate::runtime::environment::{base_environment, EnvironmentSource};

/// Installer lock file the transactions create; it is not part of the tree a
/// rollback restores.
const LOCK_NAME: &str = ".pohunek-integration.lock";

/// The hook file names, one per event Cline runs a reporter for.
const FILES: [&str; 6] = [
    "TaskStart.sh",
    "TaskResume.sh",
    "TaskComplete.sh",
    "TaskError.sh",
    "TaskCancel.sh",
    "SessionShutdown.sh",
];

/// Content of a file a user placed at a hook path.
const FOREIGN: &str = "#!/bin/sh\necho user hook\n";

struct Rig {
    root: TestDir,
    homes: ConfigHomes,
}

impl Rig {
    fn new(profiles: &[(&str, String)]) -> Self {
        let root = scoped_dir("cline-homes");
        let home = root.join("home");
        let agents = root.join("agents");
        fs::create_dir_all(&home).expect("create the user home");
        fs::create_dir_all(&agents).expect("create the agents directory");
        for (name, body) in profiles {
            fs::write(agents.join(format!("{name}.toml")), body).expect("write a profile");
        }
        let variables: Vec<(OsString, OsString)> =
            vec![(OsString::from("HOME"), home.as_os_str().to_owned())];
        let allowlist: Vec<String> = DEFAULT_ENVIRONMENT_ALLOWLIST
            .iter()
            .copied()
            .map(str::to_owned)
            .collect();
        let base = base_environment(&allowlist, &EnvironmentSource::fixed(variables))
            .expect("a base environment");
        let host = cline_shaped_host(Path::new("/bin/sh"), &[]);
        let registry = ProfileRegistry::with_runtimes(Some(agents), host);
        Self {
            root,
            homes: ConfigHomes::new(registry, base),
        }
    }

    /// The runtime's own config home, `$HOME/.cline`, created.
    fn home(&self) -> PathBuf {
        let dir = self.root.join("home/.cline");
        fs::create_dir_all(&dir).expect("create the config home");
        dir
    }

    fn install(
        &self,
        selection: &HomeSelection,
    ) -> Result<IntegrationInstallResult, ProtocolError> {
        install_in(
            &self.homes,
            Some(&cline()),
            selection,
            &RetainedSchemas::default(),
        )
    }

    fn status(&self, selection: &HomeSelection) -> protocol::IntegrationAgentStatus {
        let (profile, all_profiles) = match selection {
            HomeSelection::Runtime => (None, false),
            HomeSelection::Profile(name) => (Some(name.clone()), false),
            HomeSelection::All => (None, true),
        };
        status_in(
            &self.homes,
            IntegrationStatusParams {
                agent: Some(cline()),
                profile,
                all_profiles,
            },
        )
        .expect("status")
        .agents
        .into_iter()
        .next()
        .expect("one report")
    }
}

fn cline() -> RuntimeRef {
    RuntimeRef::from_wire("cline")
}

fn profile_text(dir: &Path) -> String {
    format!(
        "base = \"cline\"\npackage = \"pohunek.runtime.cline\"\ndigest = \"{PACKAGE_DIGEST}\"\n[env]\nCLINE_DIR = \"{}\"\n",
        dir.display()
    )
}

fn hook(dir: &Path, name: &str) -> PathBuf {
    dir.join("hooks").join(name)
}

fn tree(root: &Path) -> Vec<(PathBuf, u32, Vec<u8>)> {
    tree_snapshot(root)
        .into_iter()
        .filter(|(path, _mode, _content)| path.file_name() != Some(OsStr::new(LOCK_NAME)))
        .collect()
}

fn uninstall(rig: &Rig) -> protocol::IntegrationUninstallReport {
    uninstall_in(&rig.homes, &cline(), &HomeSelection::Runtime)
        .expect("uninstall")
        .uninstalled
        .remove(0)
}

#[test]
fn a_fresh_install_writes_six_owned_executables_and_uninstall_removes_exactly_them() {
    let rig = Rig::new(&[]);
    let dir = rig.home();

    let result = rig.install(&HomeSelection::Runtime).expect("install");

    assert_eq!(
        result.installed[0].hook_path,
        dir.join("hooks").display().to_string()
    );
    assert_eq!(result.installed[0].config_paths.len(), 6);
    assert_eq!(
        fs::metadata(dir.join("hooks"))
            .expect("hooks dir")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let first = fs::read(hook(&dir, FILES[0])).expect("read a hook");
    let text = String::from_utf8(first.clone()).expect("utf-8 reporter");
    assert!(text.starts_with("#!/bin/sh\n"));
    assert!(text
        .lines()
        .any(|line| line == "# POHUNEK_INTEGRATION_ID=cline"));
    assert!(text
        .lines()
        .any(|line| line == "# POHUNEK_INTEGRATION_VERSION=1"));
    for name in FILES {
        let path = hook(&dir, name);
        assert_eq!(fs::read(&path).expect("read a hook"), first, "{name}");
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o755
        );
    }
    let status = rig.status(&HomeSelection::Runtime);
    assert_eq!(
        status.state,
        IntegrationInstallState::Current,
        "{:?}",
        status.warnings
    );
    assert_eq!(status.installed_version, Some(1));
    assert_eq!(status.expected_asset_paths.len(), 6);
    assert_eq!(status.present_asset_paths.len(), 6);

    let second = rig.install(&HomeSelection::Runtime).expect("install again");
    assert!(second.failed.is_empty());
    assert_eq!(fs::read(hook(&dir, FILES[3])).expect("read a hook"), first);

    let report = uninstall(&rig);
    assert_eq!(report.state, IntegrationUninstallState::Removed);
    assert_eq!(report.removed_paths.len(), 6);
    assert!(report.preserved_paths.is_empty());
    for name in FILES {
        assert!(!hook(&dir, name).exists(), "{name}");
    }
    assert_eq!(
        rig.status(&HomeSelection::Runtime).state,
        IntegrationInstallState::NotInstalled
    );
}

#[test]
fn an_older_drifted_or_partial_install_is_outdated_and_a_reinstall_repairs_it() {
    let rig = Rig::new(&[]);
    let dir = rig.home();
    rig.install(&HomeSelection::Runtime).expect("install");

    let path = hook(&dir, "TaskStart.sh");
    let text = fs::read_to_string(&path).expect("read");
    fs::write(&path, text.replace("VERSION=1", "VERSION=0")).expect("older marker");
    let status = rig.status(&HomeSelection::Runtime);
    assert_eq!(status.state, IntegrationInstallState::Outdated);
    assert_eq!(status.recovery, IntegrationRecovery::Reinstall);
    assert!(status
        .warnings
        .iter()
        .any(|warning| warning.contains("TaskStart.sh")));

    fs::write(hook(&dir, "TaskError.sh"), format!("{text}# edited\n")).expect("drift");
    fs::remove_file(hook(&dir, "SessionShutdown.sh")).expect("remove one");
    let status = rig.status(&HomeSelection::Runtime);
    assert_eq!(status.state, IntegrationInstallState::Outdated);
    assert_eq!(status.recovery, IntegrationRecovery::Reinstall);

    rig.install(&HomeSelection::Runtime).expect("repair");
    let status = rig.status(&HomeSelection::Runtime);
    assert_eq!(
        status.state,
        IntegrationInstallState::Current,
        "{:?}",
        status.warnings
    );
}

#[test]
fn a_foreign_file_at_a_hook_path_refuses_the_install_and_survives_uninstall_byte_identical() {
    let rig = Rig::new(&[]);
    let dir = rig.home();
    fs::create_dir_all(dir.join("hooks")).expect("hooks dir");
    fs::write(hook(&dir, "TaskResume.sh"), FOREIGN).expect("foreign hook");
    let before = tree(&dir);

    let error = rig
        .install(&HomeSelection::Runtime)
        .expect_err("a conflict");

    assert_eq!(error.code, "integration_hook_path_conflict");
    assert!(error.msg.contains("TaskResume.sh"), "{}", error.msg);
    assert_eq!(tree(&dir), before, "nothing was written");
    let status = rig.status(&HomeSelection::Runtime);
    assert_eq!(status.state, IntegrationInstallState::Outdated);
    assert_eq!(status.recovery, IntegrationRecovery::RepairConfiguration);
    assert!(status
        .warnings
        .iter()
        .any(|warning| warning.contains("TaskResume.sh")));
    let doctor = doctor_in_with(
        &rig.homes,
        IntegrationDoctorParams {
            agent: Some(cline()),
            profile: None,
            all_profiles: false,
        },
        &[],
        &[],
    )
    .expect("doctor");
    assert!(!doctor.ok, "{doctor:?}");

    // The same path taken after an install: uninstall leaves the user's file.
    fs::remove_file(hook(&dir, "TaskResume.sh")).expect("clear the path");
    rig.install(&HomeSelection::Runtime).expect("install");
    fs::write(hook(&dir, "TaskResume.sh"), FOREIGN).expect("foreign hook replaces ours");
    let report = uninstall(&rig);
    assert_eq!(report.removed_paths.len(), 5);
    assert_eq!(
        report.preserved_paths,
        [hook(&dir, "TaskResume.sh").display().to_string()]
    );
    assert_eq!(
        fs::read_to_string(hook(&dir, "TaskResume.sh")).expect("read"),
        FOREIGN
    );
}

#[test]
fn hook_files_of_the_same_event_under_other_names_are_not_a_collision() {
    let rig = Rig::new(&[]);
    let dir = rig.home();
    fs::create_dir_all(dir.join("hooks")).expect("hooks dir");
    let others = [
        "TaskResume.py",
        "TaskResume",
        "taskresume.bash",
        "PreToolUse.sh",
    ];
    for name in others {
        fs::write(hook(&dir, name), FOREIGN).expect("foreign hook");
    }

    rig.install(&HomeSelection::Runtime)
        .expect("install beside them");

    assert_eq!(
        rig.status(&HomeSelection::Runtime).state,
        IntegrationInstallState::Current
    );
    uninstall(&rig);
    for name in others {
        assert_eq!(
            fs::read_to_string(hook(&dir, name)).expect("read"),
            FOREIGN,
            "{name}"
        );
    }
}

#[test]
fn a_symlink_or_directory_at_a_hook_path_refuses_the_install_without_a_write() {
    for kind in ["symlink", "directory"] {
        let rig = Rig::new(&[]);
        let dir = rig.home();
        fs::create_dir_all(dir.join("hooks")).expect("hooks dir");
        let target = rig.root.join("elsewhere.sh");
        fs::write(&target, FOREIGN).expect("target");
        if kind == "symlink" {
            symlink(&target, hook(&dir, "TaskStart.sh")).expect("symlink");
        } else {
            fs::create_dir(hook(&dir, "TaskStart.sh")).expect("directory");
        }
        let before = tree(&dir);

        let error = rig.install(&HomeSelection::Runtime).expect_err(kind);
        assert_eq!(error.code, "integration_hook_path_conflict", "{kind}");
        assert!(error.msg.contains("TaskStart.sh"), "{kind}: {}", error.msg);

        assert_eq!(tree(&dir), before, "{kind}: nothing was written");
        assert_eq!(fs::read_to_string(&target).expect("read"), FOREIGN);
        let report = uninstall(&rig);
        assert_eq!(
            report.state,
            IntegrationUninstallState::NotInstalled,
            "{kind}"
        );
        assert_eq!(tree(&dir), before, "{kind}: uninstall left the path alone");
    }
}

#[test]
fn a_missing_config_directory_is_not_installed_and_an_install_names_it() {
    let rig = Rig::new(&[]);

    let error = rig
        .install(&HomeSelection::Runtime)
        .expect_err("no directory");

    assert_eq!(error.code, "agent_config_dir_missing");
    let status = rig.status(&HomeSelection::Runtime);
    assert!(!status.available);
    assert_eq!(status.state, IntegrationInstallState::NotInstalled);
    assert_eq!(
        uninstall(&rig).state,
        IntegrationUninstallState::NotInstalled
    );
    assert!(
        !rig.root.join("home/.cline").exists(),
        "no directory is created"
    );
}

#[test]
fn a_failure_after_k_files_restores_the_exact_prior_tree() {
    for fail_at in 0..FILES.len() {
        let rig = Rig::new(&[]);
        let dir = rig.home();
        let before = tree(&dir);

        let result = install_in_gated(
            &rig.homes,
            Some(&cline()),
            &HomeSelection::Runtime,
            &RetainedSchemas::default(),
            &mut |_target, index, _name| {
                if index == fail_at {
                    Err(ProtocolError::new(
                        protocol::ErrorClass::Runtime,
                        "injected_step_failure",
                        "injected failure",
                        None,
                    ))
                } else {
                    Ok(())
                }
            },
        );

        assert_eq!(
            result.expect_err("the gate fails").code,
            "injected_step_failure"
        );
        assert_eq!(tree(&dir), before, "failing at step {fail_at}");
    }
}

#[test]
fn all_profiles_install_one_transaction_per_home_and_a_conflict_stays_in_its_home() {
    let rig = Rig::new(&[]);
    let personal = rig.root.join("personal");
    let work = rig.root.join("work");
    for (name, dir) in [("personal", &personal), ("work", &work)] {
        fs::create_dir_all(dir).expect("profile home");
        fs::write(
            rig.root.join(format!("agents/{name}.toml")),
            profile_text(dir),
        )
        .expect("write a profile");
    }
    fs::create_dir_all(work.join("hooks")).expect("hooks dir");
    fs::write(hook(&work, "TaskError.sh"), FOREIGN).expect("foreign hook");
    let work_before = tree(&work);
    // The runtime's own home is absent and is skipped, not created.
    let rig = Rig::new(&[
        ("personal", profile_text(&personal)),
        ("work", profile_text(&work)),
    ]);

    let result = rig.install(&HomeSelection::All).expect("per-home result");

    assert_eq!(result.installed.len(), 1);
    assert_eq!(
        result.installed[0].home.as_ref().expect("label").profiles,
        ["personal"]
    );
    assert_eq!(result.failed.len(), 1);
    assert_eq!(result.failed[0].home.profiles, ["work"]);
    assert_eq!(
        result.failed[0].error.code,
        "integration_hook_path_conflict"
    );
    assert_eq!(tree(&work), work_before);
    assert!(hook(&personal, "TaskStart.sh").is_file());
    assert!(!rig.root.join("home/.cline").exists());
    assert_eq!(
        rig.status(&HomeSelection::Profile("personal".to_owned()))
            .state,
        IntegrationInstallState::Current
    );
}

#[test]
fn the_handler_needs_its_runtime_feature_and_is_driven_by_the_identity_subagent_schema() {
    let program = Path::new("/bin/sh");
    let without = cline_shaped_document(program, &[], r#"["placement-v1"]"#);
    assert!(
        cline_shaped_definition(&without).is_err(),
        "a descriptor that names the handler must require its feature"
    );
    cline_shaped_definition(&cline_shaped_document(program, &[], CLINE_REQUIRES))
        .expect("a descriptor that requires the feature is accepted");
    let schema = pohunek_worker_protocol::hook_schema("identity-subagent-v1").expect("schema");
    assert!(schema.supports_handler("cline-hook-v1"));
    assert!(schema.admits_subagent_provider("cline"));
    assert!(schema.admits_identity_provider("cline", Some("cline")));
}
