//! Tests of the config home resolver and of the per-home lifecycle.
//!
//! Every test builds its own [`ConfigHomes`] over a fixed base environment and
//! a private agents directory, so none reads or changes the process
//! environment.

// Rust guideline compliant 2026-10-05

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt as _};
use std::path::{Path, PathBuf};

use pohunek_test_support::process_env::ProcessEnv;
use pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST;
use protocol::{
    IntegrationDoctorParams, IntegrationHome, IntegrationInstallResult, IntegrationInstallState,
    IntegrationSelector, IntegrationStatusParams, IntegrationUninstallState, ProtocolError,
    RuntimeRef,
};

use super::{ConfigHomes, HomeSelection};
use crate::agent::host::fixture::builtin_host;
use crate::agent::ProfileRegistry;
use crate::integration::doctor::doctor_in_with;
use crate::integration::handler::{install_in, install_in_gated, status_in, RetainedSchemas};
use crate::integration::tests::{scoped_dir, tree_snapshot, TestDir};
use crate::integration::uninstall_in;
use crate::runtime::environment::{base_environment, EnvironmentSource};

/// Installer lock file the transactions create; it is not part of the tree a
/// rollback restores.
const LOCK_NAME: &str = ".pohunek-integration.lock";

/// A private root holding the user's home directory, the host profiles and
/// the config homes the profiles name.
struct Rig {
    root: TestDir,
    homes: ConfigHomes,
}

impl Rig {
    /// A rig whose daemon environment forwards `HOME` and whatever else the
    /// default allowlist forwards from `daemon_env`.
    fn new(profiles: &[(&str, String)], daemon_env: &[(&str, &Path)]) -> Self {
        Self::with_allowlist(profiles, daemon_env, &[])
    }

    /// Like [`Rig::new`], additionally allowlisting `extra_allowed` names.
    fn with_allowlist(
        profiles: &[(&str, String)],
        daemon_env: &[(&str, &Path)],
        extra_allowed: &[&str],
    ) -> Self {
        let root = scoped_dir("homes");
        let home = root.join("home");
        let agents = root.join("agents");
        fs::create_dir_all(&home).expect("create the user home");
        fs::create_dir_all(&agents).expect("create the agents directory");
        for (name, body) in profiles {
            fs::write(agents.join(format!("{name}.toml")), body).expect("write a profile");
        }
        let mut variables: Vec<(OsString, OsString)> =
            vec![(OsString::from("HOME"), home.as_os_str().to_owned())];
        variables.extend(
            daemon_env
                .iter()
                .map(|(name, value)| (OsString::from(name), value.as_os_str().to_owned())),
        );
        let allowlist: Vec<String> = DEFAULT_ENVIRONMENT_ALLOWLIST
            .iter()
            .copied()
            .chain(extra_allowed.iter().copied())
            .map(str::to_owned)
            .collect();
        let base = base_environment(&allowlist, &EnvironmentSource::fixed(variables))
            .expect("a base environment");
        let registry = ProfileRegistry::with_runtimes(Some(agents), builtin_host());
        Self {
            root,
            homes: ConfigHomes::new(registry, base),
        }
    }

    /// A directory below the rig root.
    fn dir(&self, name: &str) -> PathBuf {
        let dir = self.root.join(name);
        fs::create_dir_all(&dir).expect("create a config home");
        dir
    }
}

/// The profile text of a Claude profile whose config home is `dir`.
fn claude_profile(dir: &Path) -> String {
    format!(
        "base = \"claude\"\n[env]\nCLAUDE_CONFIG_DIR = \"{}\"\n",
        dir.display()
    )
}

/// The profile text of a Codex profile whose config home is `dir`.
fn codex_profile(dir: &Path) -> String {
    format!(
        "base = \"codex\"\n[env]\nCODEX_HOME = \"{}\"\n",
        dir.display()
    )
}

fn claude() -> RuntimeRef {
    RuntimeRef::claude()
}

fn install(
    rig: &Rig,
    agent: Option<&RuntimeRef>,
    selection: &HomeSelection,
) -> Result<IntegrationInstallResult, ProtocolError> {
    install_in(&rig.homes, agent, selection, &RetainedSchemas::default())
}

fn profile(name: &str) -> HomeSelection {
    HomeSelection::Profile(name.to_owned())
}

/// The tree below `root` without the installer lock a transaction leaves.
fn tree(root: &Path) -> Vec<(PathBuf, u32, Vec<u8>)> {
    tree_snapshot(root)
        .into_iter()
        .filter(|(path, _mode, _content)| path.file_name() != Some(OsStr::new(LOCK_NAME)))
        .collect()
}

fn state_hook(dir: &Path) -> PathBuf {
    dir.join("hooks").join("pohunek-agent-state.sh")
}

fn status_of(rig: &Rig, selection: &HomeSelection) -> protocol::IntegrationAgentStatus {
    let (profile, all_profiles) = match selection {
        HomeSelection::Runtime => (None, false),
        HomeSelection::Profile(name) => (Some(name.clone()), false),
        HomeSelection::All => (None, true),
    };
    status_in(
        &rig.homes,
        IntegrationStatusParams {
            agent: Some(claude()),
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

#[test]
fn the_selection_parameters_name_one_selection_and_refuse_both() {
    assert_eq!(
        HomeSelection::from_params(None, false),
        Ok(HomeSelection::Runtime)
    );
    assert_eq!(
        HomeSelection::from_params(Some("work".to_owned()), false),
        Ok(HomeSelection::Profile("work".to_owned()))
    );
    assert_eq!(
        HomeSelection::from_params(None, true),
        Ok(HomeSelection::All)
    );
    let error = HomeSelection::from_params(Some("work".to_owned()), true).expect_err("both");
    assert_eq!(error.code, "bad_request");
}

#[test]
fn the_runtime_home_is_the_declared_default_below_the_launch_home() {
    let rig = Rig::new(&[], &[]);

    let targets = rig
        .homes
        .targets(Some(&claude()), &HomeSelection::Runtime)
        .expect("targets");

    assert_eq!(targets.len(), 1);
    assert_eq!(
        targets[0].dir().expect("resolved"),
        rig.root.join("home/.claude")
    );
    assert!(
        targets[0].label.is_none(),
        "an unselected home is unlabeled"
    );
}

#[test]
fn a_profile_home_is_the_profile_variable_and_not_the_runtime_home() {
    let work = Path::new("/profiles/work-home");
    let rig = Rig::new(&[("work", claude_profile(work))], &[]);

    let targets = rig
        .homes
        .targets(Some(&claude()), &profile("work"))
        .expect("targets");

    assert_eq!(targets[0].dir().expect("resolved"), work);
    assert_eq!(
        targets[0].label,
        Some(IntegrationHome {
            selector: Some(IntegrationSelector::Profile {
                name: "work".to_owned()
            }),
            profiles: vec!["work".to_owned()],
            bare: false,
        })
    );
}

#[test]
fn two_profiles_install_and_remove_independently() {
    let rig = Rig::new(&[], &[]);
    let work = rig.dir("work-home");
    let personal = rig.dir("personal-home");
    for (name, dir) in [("work", &work), ("personal", &personal)] {
        fs::write(
            rig.root.join(format!("agents/{name}.toml")),
            claude_profile(dir),
        )
        .expect("write a profile");
    }

    let installed = install(&rig, Some(&claude()), &profile("work")).expect("install work");
    assert_eq!(installed.installed.len(), 1);
    assert_eq!(
        installed.installed[0].home,
        Some(IntegrationHome {
            selector: Some(IntegrationSelector::Profile {
                name: "work".to_owned()
            }),
            profiles: vec!["work".to_owned()],
            bare: false,
        })
    );
    assert!(state_hook(&work).is_file(), "the work home is installed");
    assert!(
        !state_hook(&personal).exists(),
        "the other profile's home is untouched"
    );
    assert!(
        !state_hook(&rig.root.join("home/.claude")).exists(),
        "the runtime's own home is untouched"
    );

    install(&rig, Some(&claude()), &profile("personal")).expect("install personal");
    assert_eq!(
        status_of(&rig, &profile("work")).state,
        IntegrationInstallState::Current
    );
    assert_eq!(
        status_of(&rig, &profile("personal")).state,
        IntegrationInstallState::Current
    );

    let personal_before = tree(&personal);
    let removed =
        uninstall_in(&rig.homes, &claude(), &profile("work")).expect("uninstall the work home");
    assert_eq!(removed.uninstalled.len(), 1);
    assert_eq!(
        removed.uninstalled[0].state,
        IntegrationUninstallState::Removed
    );
    assert!(!state_hook(&work).exists());
    assert_eq!(
        tree(&personal),
        personal_before,
        "removing one home leaves the other as it was"
    );
    assert_eq!(
        status_of(&rig, &profile("personal")).state,
        IntegrationInstallState::Current
    );
}

#[test]
fn doctor_diagnoses_the_selected_profile_home_and_addresses_recovery_to_it() {
    let rig = Rig::new(&[], &[]);
    let work = rig.dir("work-home");
    fs::write(rig.root.join("agents/work.toml"), claude_profile(&work)).expect("write a profile");
    install(&rig, Some(&claude()), &profile("work")).expect("install");
    fs::write(state_hook(&work), "#!/bin/sh\n# tampered\n").expect("tamper with the hook");

    let report = doctor_in_with(
        &rig.homes,
        IntegrationDoctorParams {
            agent: Some(claude()),
            profile: Some("work".to_owned()),
            all_profiles: false,
        },
        &[],
        &[],
    )
    .expect("doctor");

    assert!(report.home_selectors);
    let diagnosis = &report.agents[0];
    assert!(!diagnosis.ok, "{diagnosis:?}");
    assert_eq!(
        diagnosis.home,
        Some(IntegrationHome {
            selector: Some(IntegrationSelector::Profile {
                name: "work".to_owned()
            }),
            profiles: vec!["work".to_owned()],
            bare: false,
        })
    );
    let status = diagnosis.status.as_ref().expect("status");
    assert!(status
        .expected_asset_paths
        .iter()
        .all(|path| path.starts_with(work.to_str().expect("utf-8"))));
    let fixes: Vec<&str> = diagnosis
        .findings
        .iter()
        .filter_map(|finding| finding.remediation.as_deref())
        .filter(|remediation| remediation.contains("pohunek integration install"))
        .collect();
    assert!(!fixes.is_empty(), "{diagnosis:?}");
    assert!(
        fixes
            .iter()
            .all(|fix| fix.contains("--agent claude --profile work")),
        "a recovery command reaches the profile's home: {fixes:?}"
    );
}

#[test]
fn status_recovery_commands_name_the_profile_of_the_home() {
    let rig = Rig::new(&[], &[]);
    let work = rig.dir("work-home");
    fs::write(rig.root.join("agents/work.toml"), claude_profile(&work)).expect("write a profile");
    install(&rig, Some(&claude()), &profile("work")).expect("install");
    fs::set_permissions(state_hook(&work), fs::Permissions::from_mode(0o644))
        .expect("drift the hook mode");

    let report = status_of(&rig, &profile("work"));

    let warnings = report.warnings.join("\n");
    assert!(
        warnings.contains("pohunek integration install"),
        "{warnings}"
    );
    assert!(
        warnings.contains("--agent claude --profile work"),
        "{warnings}"
    );
    let unscoped = status_of(&rig, &HomeSelection::Runtime);
    assert!(unscoped
        .warnings
        .iter()
        .all(|warning| !warning.contains("--profile")));
}

#[test]
fn an_ambient_config_variable_in_the_daemon_environment_steers_nothing() {
    let ambient = scoped_dir("ambient-provider-home");
    // The daemon process itself carries the variable, and so does the daemon
    // environment the rig forwards the default allowlist from.
    let mut process = ProcessEnv::lock();
    process.set("CLAUDE_CONFIG_DIR", ambient.as_path());
    let rig = Rig::new(&[], &[("CLAUDE_CONFIG_DIR", ambient.as_path())]);
    let work = rig.dir("work-home");
    fs::write(rig.root.join("agents/work.toml"), claude_profile(&work)).expect("write a profile");
    fs::create_dir_all(rig.root.join("home/.claude")).expect("create the default home");
    let ambient_before = tree(&ambient);

    install(&rig, Some(&claude()), &profile("work")).expect("install the profile home");
    install(&rig, Some(&claude()), &HomeSelection::Runtime).expect("install the default home");

    assert!(state_hook(&work).is_file(), "the profile's variable wins");
    assert!(
        state_hook(&rig.root.join("home/.claude")).is_file(),
        "the default home is below the launch HOME"
    );
    assert_eq!(
        tree(&ambient),
        ambient_before,
        "a variable the launch would not hand the agent steers nothing"
    );
}

#[test]
fn allowlisting_the_variable_forwards_it_to_the_runtime_home() {
    let ambient = scoped_dir("allowlisted-provider-home");
    let rig = Rig::with_allowlist(
        &[],
        &[("CLAUDE_CONFIG_DIR", ambient.as_path())],
        &["CLAUDE_CONFIG_DIR"],
    );

    let targets = rig
        .homes
        .targets(Some(&claude()), &HomeSelection::Runtime)
        .expect("targets");

    assert_eq!(
        targets[0].dir().expect("resolved"),
        ambient.as_path(),
        "the home is what the launch's own environment names"
    );
}

#[test]
fn a_tilde_in_a_profile_variable_is_refused_and_never_expanded() {
    let rig = Rig::new(
        &[(
            "tilde",
            "base = \"claude\"\n[env]\nCLAUDE_CONFIG_DIR = \"~/tilde-home\"\n".to_owned(),
        )],
        &[],
    );
    let expanded = rig.root.join("home/tilde-home");
    fs::create_dir_all(&expanded).expect("create the directory a tilde would expand to");
    let before = tree(&rig.root.join("home"));

    let error = install(&rig, Some(&claude()), &profile("tilde")).expect_err("refused");

    assert_eq!(error.code, "agent_config_dir_invalid");
    assert!(
        !error.msg.contains("tilde-home"),
        "the refused value is never named: {}",
        error.msg
    );
    assert_eq!(tree(&rig.root.join("home")), before, "nothing was written");
    let report = status_of(&rig, &profile("tilde"));
    assert!(!report.available);
    assert!(report.warnings[0].contains("agent_config_dir_invalid"));
}

#[test]
fn a_profile_of_another_runtime_or_no_profile_is_a_typed_error() {
    let rig = Rig::new(
        &[("codex-work", codex_profile(Path::new("/profiles/codex")))],
        &[],
    );

    let mismatch =
        install(&rig, Some(&claude()), &profile("codex-work")).expect_err("another runtime");
    assert_eq!(mismatch.code, "integration_profile_runtime_mismatch");

    for name in ["missing", "claude"] {
        let absent = install(&rig, Some(&claude()), &profile(name)).expect_err("no such profile");
        assert_eq!(absent.code, "agent_profile_not_found", "{name}");
    }
}

#[test]
fn a_profile_alone_selects_its_own_runtime() {
    let rig = Rig::new(&[], &[]);
    let codex_home = rig.dir("codex-home");
    fs::write(
        rig.root.join("agents/codex-work.toml"),
        codex_profile(&codex_home),
    )
    .expect("write a profile");

    let installed = install(&rig, None, &profile("codex-work")).expect("install");

    assert_eq!(installed.installed.len(), 1);
    assert_eq!(installed.installed[0].agent, RuntimeRef::codex());
    assert!(codex_home.join("pohunek-agent-state.sh").is_file());
}

/// A rig with two Claude profiles whose homes exist and hold user settings.
fn two_profile_rig() -> (Rig, PathBuf, PathBuf) {
    let rig = Rig::new(&[], &[]);
    let personal = rig.dir("personal-home");
    let work = rig.dir("work-home");
    for (name, dir) in [("personal", &personal), ("work", &work)] {
        fs::write(
            rig.root.join(format!("agents/{name}.toml")),
            claude_profile(dir),
        )
        .expect("write a profile");
        fs::write(dir.join("settings.json"), "{\"model\":\"user-model\"}\n")
            .expect("write the user's settings");
    }
    (rig, personal, work)
}

#[test]
fn all_profiles_install_one_transaction_per_distinct_home() {
    let (rig, personal, work) = two_profile_rig();

    let result = install(&rig, Some(&claude()), &HomeSelection::All).expect("install every home");

    assert!(result.failed.is_empty(), "{:?}", result.failed);
    let labels: Vec<&IntegrationHome> = result
        .installed
        .iter()
        .map(|report| report.home.as_ref().expect("a labeled report"))
        .collect();
    assert_eq!(
        labels
            .iter()
            .map(|home| home.profiles.clone())
            .collect::<Vec<_>>(),
        [vec!["personal".to_owned()], vec!["work".to_owned()]]
    );
    assert!(state_hook(&personal).is_file());
    assert!(state_hook(&work).is_file());
    assert!(
        !state_hook(&rig.root.join("home/.claude")).exists(),
        "an absent runtime home is skipped, not created"
    );
}

#[test]
fn a_conflict_in_one_home_is_reported_and_does_not_touch_the_other() {
    let (rig, personal, work) = two_profile_rig();
    fs::write(personal.join("settings.json"), "{ not json").expect("break the settings");
    let personal_before = tree(&personal);

    let result = install(&rig, Some(&claude()), &HomeSelection::All).expect("per-home result");

    assert_eq!(result.failed.len(), 1);
    assert_eq!(result.failed[0].home.profiles, ["personal"]);
    assert_eq!(result.failed[0].error.code, "integration_settings_invalid");
    assert_eq!(
        tree(&personal),
        personal_before,
        "the conflicting home is exactly as it was"
    );
    assert_eq!(result.installed.len(), 1, "the other home still installed");
    assert!(state_hook(&work).is_file());
}

#[test]
fn a_failing_home_rolls_back_to_its_exact_prior_tree_and_only_that_home() {
    for failing in ["personal", "work"] {
        let (rig, personal, work) = two_profile_rig();
        let (failed_dir, other_dir) = if failing == "personal" {
            (&personal, &work)
        } else {
            (&work, &personal)
        };
        let failed_before = tree(failed_dir);

        let result = install_in_gated(
            &rig.homes,
            Some(&claude()),
            &HomeSelection::All,
            &RetainedSchemas::default(),
            &mut |target, index, _name| {
                let named = target
                    .label
                    .as_ref()
                    .is_some_and(|home| home.profiles == [failing]);
                if named && index == 1 {
                    Err(ProtocolError::new(
                        protocol::ErrorClass::Runtime,
                        "injected_failure",
                        "injected failure",
                        None,
                    ))
                } else {
                    Ok(())
                }
            },
        )
        .expect("per-home result");

        assert_eq!(result.failed.len(), 1, "{failing}");
        assert_eq!(result.failed[0].error.code, "injected_failure");
        assert_eq!(
            tree(failed_dir),
            failed_before,
            "{failing}: the failed home is restored exactly"
        );
        assert_eq!(result.installed.len(), 1, "{failing}");
        assert!(
            state_hook(other_dir).is_file(),
            "{failing}: the other home keeps its committed install"
        );
    }
}

#[test]
fn profiles_resolving_to_one_canonical_home_share_one_transaction() {
    let rig = Rig::new(&[], &[]);
    let real = rig.dir("real-home");
    let link = rig.root.join("a-link");
    symlink(&real, &link).expect("link to the real home");
    // `a-link` sorts before `b-real`, so the symlinked selector comes first.
    fs::write(rig.root.join("agents/a-link.toml"), claude_profile(&link)).expect("write");
    fs::write(rig.root.join("agents/b-real.toml"), claude_profile(&real)).expect("write");

    let result = install(&rig, Some(&claude()), &HomeSelection::All).expect("install");

    assert!(result.failed.is_empty(), "{:?}", result.failed);
    assert_eq!(result.installed.len(), 1, "one home, one transaction");
    assert_eq!(
        result.installed[0].home,
        Some(IntegrationHome {
            selector: Some(IntegrationSelector::Profile {
                name: "b-real".to_owned()
            }),
            profiles: vec!["a-link".to_owned(), "b-real".to_owned()],
            bare: false,
        })
    );
    assert!(
        result.installed[0]
            .hook_path
            .starts_with(real.to_str().expect("utf-8")),
        "the real directory is written, never through the symlink: {}",
        result.installed[0].hook_path
    );
    let statuses = status_in(
        &rig.homes,
        IntegrationStatusParams {
            agent: Some(claude()),
            all_profiles: true,
            ..IntegrationStatusParams::default()
        },
    )
    .expect("status");
    let present: Vec<&protocol::IntegrationAgentStatus> = statuses
        .agents
        .iter()
        .filter(|report| report.available)
        .collect();
    assert_eq!(present.len(), 1, "one report for the shared home");
}

#[test]
fn recovery_commands_of_a_shared_home_name_the_selector_that_reaches_the_real_directory() {
    let rig = Rig::new(&[], &[]);
    let real = rig.dir("real-home");
    let link = rig.root.join("a-link");
    symlink(&real, &link).expect("link to the real home");
    fs::write(rig.root.join("agents/a-link.toml"), claude_profile(&link)).expect("write");
    fs::write(rig.root.join("agents/b-real.toml"), claude_profile(&real)).expect("write");
    install(&rig, Some(&claude()), &HomeSelection::All).expect("install");
    fs::set_permissions(state_hook(&real), fs::Permissions::from_mode(0o644))
        .expect("drift the hook mode");

    let reports = status_in(
        &rig.homes,
        IntegrationStatusParams {
            agent: Some(claude()),
            all_profiles: true,
            ..IntegrationStatusParams::default()
        },
    )
    .expect("status")
    .agents;

    let shared = reports
        .iter()
        .find(|report| report.available)
        .expect("the shared home is reported");
    let warnings = shared.warnings.join("\n");
    assert!(
        warnings.contains("--agent claude --profile b-real"),
        "{warnings}"
    );
    assert!(
        !warnings.contains("a-link"),
        "a command that resolves to the refused symlink is never suggested: {warnings}"
    );
}

#[test]
fn a_symlinked_runtime_home_hands_the_selector_to_the_profile_with_the_real_directory() {
    let rig = Rig::new(&[], &[]);
    let real = rig.dir("real-home");
    symlink(&real, rig.root.join("home/.claude")).expect("link the default home to it");
    fs::write(rig.root.join("agents/real.toml"), claude_profile(&real)).expect("write");

    let result = install(&rig, Some(&claude()), &HomeSelection::All).expect("install");

    assert!(result.failed.is_empty(), "{:?}", result.failed);
    let home = result.installed[0].home.clone().expect("labeled");
    assert!(
        home.bare,
        "the default home resolves to the shared directory"
    );
    assert_eq!(
        home.selector,
        Some(IntegrationSelector::Profile {
            name: "real".to_owned()
        }),
        "the default path is the refused symlink, so recovery must name the profile"
    );
}

#[test]
fn the_missing_directory_error_names_each_directory_and_runtime_once() {
    let rig = Rig::new(&[], &[]);
    for name in ["one", "two"] {
        fs::write(
            rig.root.join(format!("agents/{name}.toml")),
            claude_profile(&rig.root.join(format!("absent-{name}"))),
        )
        .expect("write a profile");
    }

    let error = install(&rig, Some(&claude()), &HomeSelection::All).expect_err("nothing exists");

    assert_eq!(error.code, "agent_config_dir_missing");
    assert_eq!(error.recover.as_deref(), Some("install Claude Code first"));
    assert_eq!(error.msg.matches("absent-one").count(), 1, "{}", error.msg);
    assert_eq!(error.msg.matches("absent-two").count(), 1, "{}", error.msg);
}

#[test]
fn the_runtime_home_and_a_profile_naming_it_are_one_home() {
    let rig = Rig::new(&[], &[]);
    let default_home = rig.dir("home/.claude");
    fs::write(
        rig.root.join("agents/same.toml"),
        claude_profile(&default_home),
    )
    .expect("write a profile");

    let result = install(&rig, Some(&claude()), &HomeSelection::All).expect("install");

    assert_eq!(result.installed.len(), 1);
    assert_eq!(
        result.installed[0].home,
        Some(IntegrationHome {
            selector: Some(IntegrationSelector::Default),
            profiles: vec!["same".to_owned()],
            bare: true,
        })
    );
}

#[test]
fn all_profiles_without_any_existing_home_is_a_missing_directory_error() {
    let rig = Rig::new(&[], &[]);

    let error = install(&rig, Some(&claude()), &HomeSelection::All).expect_err("nothing exists");

    assert_eq!(error.code, "agent_config_dir_missing");
}

#[test]
fn an_unresolvable_profile_value_is_a_failure_of_its_home_not_of_the_request() {
    let (rig, _personal, work) = two_profile_rig();
    fs::write(
        rig.root.join("agents/relative.toml"),
        "base = \"claude\"\n[env]\nCLAUDE_CONFIG_DIR = \"relative/home\"\n",
    )
    .expect("write a profile");

    let result = install(&rig, Some(&claude()), &HomeSelection::All).expect("per-home result");

    assert_eq!(result.failed.len(), 1);
    assert_eq!(result.failed[0].home.profiles, ["relative"]);
    assert_eq!(result.failed[0].error.code, "agent_config_dir_invalid");
    assert!(state_hook(&work).is_file());
}

#[test]
fn all_profiles_without_an_agent_covers_every_runtime_with_a_present_home() {
    let (rig, _personal, _work) = two_profile_rig();
    let codex_home = rig.dir("codex-home");
    fs::write(
        rig.root.join("agents/codex-work.toml"),
        codex_profile(&codex_home),
    )
    .expect("write a profile");

    let result = install(&rig, None, &HomeSelection::All).expect("install");

    let agents: Vec<&str> = result
        .installed
        .iter()
        .map(|report| report.agent.as_wire())
        .collect();
    assert_eq!(agents, ["claude", "claude", "codex"]);
    assert!(codex_home.join("pohunek-agent-state.sh").is_file());
}

#[test]
fn uninstall_all_profiles_removes_per_home_and_reports_a_failing_home() {
    let (rig, personal, work) = two_profile_rig();
    install(&rig, Some(&claude()), &HomeSelection::All).expect("install every home");
    // A symlinked config directory is refused by the removal.
    let moved = rig.root.join("moved-personal");
    fs::rename(&personal, &moved).expect("move the home");
    symlink(&moved, &personal).expect("link the old path to it");

    let result = uninstall_in(&rig.homes, &claude(), &HomeSelection::All).expect("per-home result");

    assert_eq!(result.failed.len(), 1);
    assert_eq!(result.failed[0].home.profiles, ["personal"]);
    let removed: Vec<&protocol::IntegrationUninstallReport> = result
        .uninstalled
        .iter()
        .filter(|report| report.state == IntegrationUninstallState::Removed)
        .collect();
    assert_eq!(removed.len(), 1);
    assert_eq!(
        removed[0].home.as_ref().map(|home| home.profiles.clone()),
        Some(vec!["work".to_owned()])
    );
    assert!(!state_hook(&work).exists());
    assert!(
        state_hook(&moved).is_file(),
        "the refused home is untouched"
    );
}

#[test]
fn status_all_profiles_labels_every_home_and_degrades_an_unresolvable_one() {
    let (rig, _personal, _work) = two_profile_rig();
    fs::write(
        rig.root.join("agents/relative.toml"),
        "base = \"claude\"\n[env]\nCLAUDE_CONFIG_DIR = \"relative/home\"\n",
    )
    .expect("write a profile");

    let result = status_in(
        &rig.homes,
        IntegrationStatusParams {
            agent: Some(claude()),
            all_profiles: true,
            ..IntegrationStatusParams::default()
        },
    )
    .expect("status");
    assert!(
        result.home_selectors,
        "a client proves selector support from this marker"
    );
    let reports = result.agents;

    let labeled: Vec<(Vec<String>, bool, bool)> = reports
        .iter()
        .map(|report| {
            let home = report.home.as_ref().expect("labeled");
            (home.profiles.clone(), home.bare, report.available)
        })
        .collect();
    assert_eq!(
        labeled,
        [
            (Vec::new(), true, false),
            (vec!["personal".to_owned()], false, true),
            (vec!["relative".to_owned()], false, false),
            (vec!["work".to_owned()], false, true),
        ]
    );
}

#[tokio::test]
async fn the_registry_resolves_homes_from_its_own_launch_environment() {
    use crate::session::{SessionRegistry, SessionRegistryConfig};

    let ambient = scoped_dir("registry-ambient");
    let root = scoped_dir("registry-root");
    let home = root.join("home");
    let agents = root.join("agents");
    let work = root.join("work-home");
    for dir in [&home, &agents, &work] {
        fs::create_dir_all(dir).expect("create a directory");
    }
    fs::write(agents.join("work.toml"), claude_profile(&work)).expect("write a profile");
    let environment = |allowlist: &[&str]| {
        SessionRegistry::new_with_runtimes_and_environment(
            SessionRegistryConfig {
                agents_dir: Some(agents.clone()),
                ..SessionRegistryConfig::default()
            },
            builtin_host(),
            EnvironmentSource::fixed([
                (OsString::from("HOME"), home.as_os_str().to_owned()),
                (
                    OsString::from("CLAUDE_CONFIG_DIR"),
                    ambient.as_os_str().to_owned(),
                ),
            ]),
            allowlist.iter().map(|name| (*name).to_owned()).collect(),
        )
    };

    let narrow = environment(&["HOME"]).integration_homes().expect("homes");
    let runtime_home = |homes: &ConfigHomes| {
        homes
            .targets(Some(&claude()), &HomeSelection::Runtime)
            .expect("targets")[0]
            .dir()
            .expect("resolved")
            .to_path_buf()
    };
    assert_eq!(runtime_home(&narrow), home.join(".claude"));
    let profile_home = narrow
        .targets(Some(&claude()), &profile("work"))
        .expect("targets")[0]
        .dir()
        .expect("resolved")
        .to_path_buf();
    assert_eq!(profile_home, work);

    let forwarding = environment(&["HOME", "CLAUDE_CONFIG_DIR"])
        .integration_homes()
        .expect("homes");
    assert_eq!(
        runtime_home(&forwarding),
        ambient.as_path(),
        "the registry's own allowlist decides what reaches the agent"
    );
}
