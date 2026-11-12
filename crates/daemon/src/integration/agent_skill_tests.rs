//! Behavioural tests of the opt-in agent skill install, over real fixture
//! homes, the official Claude and Codex descriptors and the same per-home
//! lifecycle `integration.install`, `.status` and `.uninstall` run.

// Rust guideline compliant 2026-11-11

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::{symlink, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use knowledge::{agent_skill_sha256, AGENT_SKILL};
use pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST;
use protocol::{
    AgentSkillChange, AgentSkillRemoval, AgentSkillState, IntegrationAgentStatus,
    IntegrationInstallResult, IntegrationStatusParams, IntegrationUninstallResult,
    IntegrationUninstallState, ProtocolError, RuntimeRef,
};

use super::{ConfigHomes, HomeSelection};
use crate::agent::host::fixture::{
    cline_shaped_and_official_host, cline_shaped_host, official_host, official_profile_pin,
    pi_shaped_host, pi_shaped_profile_pin, PI_SHAPED_NO_CHECK,
};
use crate::agent::host::RuntimeHost;
use crate::agent::ProfileRegistry;
use crate::integration::handler::{install_in_with, status_in, RetainedSchemas};
use crate::integration::tests::{scoped_dir, TestDir};
use crate::integration::uninstall_in_with;
use crate::runtime::environment::{base_environment, EnvironmentSource};

/// File mode of the skill and its marker.
const FILE_MODE: u32 = 0o600;

/// Mode of a directory the install creates.
const DIR_MODE: u32 = 0o700;

/// A user home, the host profiles and the config homes the profiles name.
struct Rig {
    root: TestDir,
    homes: ConfigHomes,
}

impl Rig {
    fn new(profiles: &[(&str, String)]) -> Self {
        Self::with_host(profiles, official_host())
    }

    fn with_host(profiles: &[(&str, String)], host: RuntimeHost) -> Self {
        let root = scoped_dir("skill");
        let home = root.join("home");
        let agents = root.join("agents");
        for dir in [home.join(".claude"), home.join(".codex"), agents.clone()] {
            fs::create_dir_all(dir).expect("create a fixture directory");
        }
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
        let registry = ProfileRegistry::with_runtimes(Some(agents), host);
        Self {
            root,
            homes: ConfigHomes::new(registry, base),
        }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// A config home directory below the rig root.
    fn dir(&self, name: &str) -> PathBuf {
        let dir = self.root.join(name);
        fs::create_dir_all(&dir).expect("create a config home");
        dir
    }

    fn install(
        &self,
        agent: &RuntimeRef,
        selection: &HomeSelection,
        skill: bool,
    ) -> Result<IntegrationInstallResult, ProtocolError> {
        install_in_with(
            &self.homes,
            Some(agent),
            selection,
            &RetainedSchemas::default(),
            skill,
        )
    }

    fn uninstall(
        &self,
        agent: &RuntimeRef,
        selection: &HomeSelection,
        skill_only: bool,
    ) -> IntegrationUninstallResult {
        uninstall_in_with(&self.homes, agent, selection, skill_only).expect("uninstall")
    }

    fn statuses(&self, agent: &RuntimeRef, all_profiles: bool) -> Vec<IntegrationAgentStatus> {
        status_in(
            &self.homes,
            IntegrationStatusParams {
                agent: Some(agent.clone()),
                profile: None,
                all_profiles,
            },
        )
        .expect("status")
        .agents
    }
}

fn claude_profile(dir: &Path) -> String {
    format!(
        "base = \"claude\"\n{}[env]\nCLAUDE_CONFIG_DIR = \"{}\"\n",
        official_profile_pin("claude"),
        dir.display()
    )
}

fn codex_profile(dir: &Path) -> String {
    format!(
        "base = \"codex\"\n{}[env]\nCODEX_HOME = \"{}\"\n",
        official_profile_pin("codex"),
        dir.display()
    )
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

fn claude_skill_dir(config_home: &Path) -> PathBuf {
    config_home.join("skills/pohunek")
}

fn codex_skill_dir(user_home: &Path) -> PathBuf {
    user_home.join(".agents/skills/pohunek")
}

fn marker_sha(skill_dir: &Path) -> String {
    let marker: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(skill_dir.join(".pohunek-managed")).expect("read the marker"),
    )
    .expect("the marker is JSON");
    marker["sha256"].as_str().expect("a sha256").to_owned()
}

fn skill_report(result: &IntegrationInstallResult) -> protocol::AgentSkillReport {
    result.installed[0].skill.clone().expect("a skill report")
}

fn skill_status(status: &IntegrationAgentStatus) -> protocol::AgentSkillStatus {
    status.skill.clone().expect("a skill status")
}

/// Writes the foreign or stale skill body `text` with a marker for `sha`.
fn plant(skill_dir: &Path, text: &str, marker_sha: Option<&str>) {
    fs::create_dir_all(skill_dir).expect("create the skill directory");
    fs::write(skill_dir.join("SKILL.md"), text).expect("write the skill");
    fs::set_permissions(
        skill_dir.join("SKILL.md"),
        fs::Permissions::from_mode(FILE_MODE),
    )
    .expect("skill mode");
    if let Some(sha) = marker_sha {
        let marker = format!("{{\"schema\":1,\"skill\":\"pohunek\",\"sha256\":\"{sha}\"}}\n");
        fs::write(skill_dir.join(".pohunek-managed"), marker).expect("write the marker");
        fs::set_permissions(
            skill_dir.join(".pohunek-managed"),
            fs::Permissions::from_mode(FILE_MODE),
        )
        .expect("marker mode");
    }
}

fn sha_of(text: &str) -> String {
    use sha2::{Digest as _, Sha256};
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

#[test]
fn install_writes_the_skill_and_its_marker_only_when_asked() {
    let rig = Rig::new(&[]);
    let config = rig.home().join(".claude");

    rig.install(&RuntimeRef::claude(), &HomeSelection::Runtime, false)
        .expect("install the hooks");
    assert!(!config.join("skills").exists(), "no skill without --skill");
    assert!(skill_report_absent(&rig));

    let result = rig
        .install(&RuntimeRef::claude(), &HomeSelection::Runtime, true)
        .expect("install with the skill");
    let report = skill_report(&result);
    assert_eq!(report.state, AgentSkillChange::Created);
    assert_eq!(report.found, AgentSkillState::Absent);
    let dir = claude_skill_dir(&config);
    assert_eq!(
        fs::read_to_string(dir.join("SKILL.md")).expect("read the skill"),
        AGENT_SKILL
    );
    assert_eq!(marker_sha(&dir), agent_skill_sha256());
    assert_eq!(mode_of(&dir.join("SKILL.md")), FILE_MODE);
    assert_eq!(mode_of(&dir.join(".pohunek-managed")), FILE_MODE);
    assert_eq!(mode_of(&dir), DIR_MODE);
    assert_eq!(mode_of(&config.join("skills")), DIR_MODE);

    let again = rig
        .install(&RuntimeRef::claude(), &HomeSelection::Runtime, true)
        .expect("install again");
    assert_eq!(skill_report(&again).state, AgentSkillChange::Unchanged);
}

/// Whether the status of the runtime's own home reports no installed skill.
fn skill_report_absent(rig: &Rig) -> bool {
    skill_status(&rig.statuses(&RuntimeRef::claude(), false)[0]).state == AgentSkillState::Absent
}

#[test]
fn update_refreshes_a_marked_skill_of_another_version() {
    let rig = Rig::new(&[]);
    let dir = claude_skill_dir(&rig.home().join(".claude"));
    let old = "# an older skill\n";
    plant(&dir, old, Some(&sha_of(old)));

    let status = skill_status(&rig.statuses(&RuntimeRef::claude(), false)[0]);
    assert_eq!(status.state, AgentSkillState::Stale);

    let result = rig
        .install(&RuntimeRef::claude(), &HomeSelection::Runtime, true)
        .expect("refresh");
    let report = skill_report(&result);
    assert_eq!(report.state, AgentSkillChange::Refreshed);
    assert_eq!(report.found, AgentSkillState::Stale);
    assert_eq!(
        fs::read_to_string(dir.join("SKILL.md")).expect("read"),
        AGENT_SKILL
    );
    assert_eq!(marker_sha(&dir), agent_skill_sha256());
    assert_eq!(mode_of(&dir.join("SKILL.md")), FILE_MODE);
}

#[test]
fn a_foreign_skill_is_left_alone_reported_and_the_hooks_are_still_installed() {
    let rig = Rig::new(&[]);
    let config = rig.home().join(".claude");
    let dir = claude_skill_dir(&config);
    let foreign = "---\nname: pohunek\n---\nmy own notes\n";
    plant(&dir, foreign, None);
    let inode = fs::metadata(dir.join("SKILL.md")).expect("metadata").ino();

    for _ in 0..2 {
        let result = rig
            .install(&RuntimeRef::claude(), &HomeSelection::Runtime, true)
            .expect("install");
        let report = skill_report(&result);
        assert_eq!(report.state, AgentSkillChange::Refused);
        assert_eq!(report.found, AgentSkillState::Foreign);
        assert_eq!(
            report.error.expect("a reason").code,
            "integration_agent_skill_foreign"
        );
        assert!(
            !result.installed[0].hook_path.is_empty()
                && Path::new(&result.installed[0].hook_path).exists(),
            "the hooks are installed regardless"
        );
        assert_eq!(
            fs::read_to_string(dir.join("SKILL.md")).expect("read"),
            foreign
        );
        assert_eq!(
            fs::metadata(dir.join("SKILL.md")).expect("metadata").ino(),
            inode
        );
        assert!(!dir.join(".pohunek-managed").exists());
    }
}

#[test]
fn an_edited_skill_is_reported_modified_and_left_unchanged() {
    let rig = Rig::new(&[]);
    let dir = claude_skill_dir(&rig.home().join(".claude"));
    rig.install(&RuntimeRef::claude(), &HomeSelection::Runtime, true)
        .expect("install");
    let edited = format!("{AGENT_SKILL}\nmy addition\n");
    fs::write(dir.join("SKILL.md"), &edited).expect("edit");

    let result = rig
        .install(&RuntimeRef::claude(), &HomeSelection::Runtime, true)
        .expect("update");
    let report = skill_report(&result);
    assert_eq!(report.state, AgentSkillChange::Refused);
    assert_eq!(report.found, AgentSkillState::Modified);
    assert_eq!(
        report.error.expect("a reason").code,
        "integration_agent_skill_modified"
    );
    assert_eq!(
        fs::read_to_string(dir.join("SKILL.md")).expect("read"),
        edited
    );
}

#[test]
fn uninstall_removes_only_the_marked_skill() {
    let rig = Rig::new(&[]);
    let config = rig.home().join(".claude");
    let dir = claude_skill_dir(&config);
    rig.install(&RuntimeRef::claude(), &HomeSelection::Runtime, true)
        .expect("install");
    let hook = config.join("hooks").join("pohunek-agent-state.sh");
    assert!(hook.exists());

    let only = rig.uninstall(&RuntimeRef::claude(), &HomeSelection::Runtime, true);
    assert_eq!(only.uninstalled[0].skill, Some(AgentSkillRemoval::Removed));
    assert_eq!(
        only.uninstalled[0].state,
        IntegrationUninstallState::Unchanged,
        "the hooks were left in place, not reported as absent"
    );
    assert!(!dir.exists(), "the empty skill directory is removed too");
    assert!(config.join("skills").exists(), "the skills directory stays");
    assert!(hook.exists(), "the hooks stay");

    // A foreign skill stays when the hooks are removed.
    let foreign = "someone else's skill\n";
    plant(&dir, foreign, None);
    let all = rig.uninstall(&RuntimeRef::claude(), &HomeSelection::Runtime, false);
    assert_eq!(
        all.uninstalled[0].skill,
        Some(AgentSkillRemoval::LeftForeign)
    );
    assert!(!hook.exists(), "the hooks are removed");
    assert_eq!(
        fs::read_to_string(dir.join("SKILL.md")).expect("read"),
        foreign
    );

    let nothing = rig.uninstall(&RuntimeRef::claude(), &HomeSelection::Runtime, true);
    assert_eq!(
        nothing.uninstalled[0].skill,
        Some(AgentSkillRemoval::LeftForeign)
    );
}

#[test]
fn a_full_uninstall_removes_the_marked_skill_with_the_hooks() {
    let rig = Rig::new(&[]);
    let dir = claude_skill_dir(&rig.home().join(".claude"));
    rig.install(&RuntimeRef::claude(), &HomeSelection::Runtime, true)
        .expect("install");

    let result = rig.uninstall(&RuntimeRef::claude(), &HomeSelection::Runtime, false);

    assert_eq!(
        result.uninstalled[0].skill,
        Some(AgentSkillRemoval::Removed)
    );
    assert!(!dir.exists());
}

#[test]
fn status_reports_the_skill_state_per_home() {
    let rig = Rig::new(&[]);
    let work_home = rig.dir("work-home");
    fs::write(
        rig.root.join("agents/work.toml"),
        claude_profile(&work_home),
    )
    .expect("write the profile");
    rig.install(
        &RuntimeRef::claude(),
        &HomeSelection::Profile("work".to_owned()),
        true,
    )
    .expect("install into the work home");

    let statuses = rig.statuses(&RuntimeRef::claude(), true);

    assert_eq!(statuses.len(), 2);
    let mut states: Vec<_> = statuses
        .iter()
        .map(|status| {
            let skill = skill_status(status);
            (
                status.home.as_ref().map(|home| home.bare),
                skill.state,
                skill.shared,
                skill.expected_sha256,
            )
        })
        .collect();
    states.sort_by_key(|(bare, ..)| *bare);
    assert_eq!(
        states,
        vec![
            (
                Some(false),
                AgentSkillState::Current,
                false,
                agent_skill_sha256()
            ),
            (
                Some(true),
                AgentSkillState::Absent,
                false,
                agent_skill_sha256()
            ),
        ]
    );
    let current = statuses
        .iter()
        .map(skill_status)
        .find(|status| status.state == AgentSkillState::Current)
        .expect("one current home");
    assert_eq!(
        current.installed_sha256.as_deref(),
        Some(agent_skill_sha256().as_str())
    );
    assert!(current
        .path
        .expect("a path")
        .ends_with("work-home/skills/pohunek/SKILL.md"));
}

#[test]
fn every_codex_home_shares_one_skill_below_the_user_home() {
    let rig = Rig::new(&[]);
    let first = rig.dir("codex-a");
    let second = rig.dir("codex-b");
    for (name, dir) in [("a", &first), ("b", &second)] {
        fs::write(
            rig.root.join(format!("agents/{name}.toml")),
            codex_profile(dir),
        )
        .expect("write the profile");
    }

    let result = rig
        .install(&RuntimeRef::codex(), &HomeSelection::All, true)
        .expect("install into every home");

    assert!(result.failed.is_empty(), "{:?}", result.failed);
    let shared = codex_skill_dir(&rig.home());
    assert_eq!(
        fs::read_to_string(shared.join("SKILL.md")).expect("read"),
        AGENT_SKILL
    );
    assert!(!first.join("skills").exists() && !second.join("skills").exists());
    let statuses = rig.statuses(&RuntimeRef::codex(), true);
    assert_eq!(statuses.len(), 3);
    for status in &statuses {
        let skill = skill_status(status);
        assert_eq!(skill.state, AgentSkillState::Current);
        assert!(skill.shared);
    }
}

#[test]
fn removing_one_profile_keeps_the_shared_skill_and_all_profiles_removes_it() {
    let rig = Rig::new(&[]);
    let first = rig.dir("codex-a");
    fs::write(rig.root.join("agents/a.toml"), codex_profile(&first)).expect("write the profile");
    rig.install(&RuntimeRef::codex(), &HomeSelection::All, true)
        .expect("install");
    let shared = codex_skill_dir(&rig.home());

    let kept = rig.uninstall(
        &RuntimeRef::codex(),
        &HomeSelection::Profile("a".to_owned()),
        false,
    );
    assert_eq!(
        kept.uninstalled[0].skill,
        Some(AgentSkillRemoval::KeptShared)
    );
    assert!(shared.join("SKILL.md").exists());

    let removed = rig.uninstall(&RuntimeRef::codex(), &HomeSelection::All, false);
    assert!(removed
        .uninstalled
        .iter()
        .any(|report| report.skill == Some(AgentSkillRemoval::Removed)));
    assert!(!shared.exists());
}

#[test]
fn a_symlinked_skills_directory_is_refused_and_nothing_is_written_through_it() {
    let rig = Rig::new(&[]);
    let config = rig.home().join(".claude");
    let target = rig.dir("elsewhere");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o777)).expect("writable target");
    symlink(&target, config.join("skills")).expect("link the skills directory");

    let result = rig
        .install(&RuntimeRef::claude(), &HomeSelection::Runtime, true)
        .expect("install");

    let report = skill_report(&result);
    assert_eq!(report.state, AgentSkillChange::Refused);
    assert_eq!(report.found, AgentSkillState::Unsafe);
    assert_eq!(
        report.error.expect("a reason").code,
        "integration_agent_skill_unsafe"
    );
    assert_eq!(fs::read_dir(&target).expect("list").count(), 0);
    let status = skill_status(&rig.statuses(&RuntimeRef::claude(), false)[0]);
    assert_eq!(status.state, AgentSkillState::Unsafe);
    let removal = rig.uninstall(&RuntimeRef::claude(), &HomeSelection::Runtime, true);
    assert_eq!(
        removal.uninstalled[0].skill,
        Some(AgentSkillRemoval::LeftForeign)
    );
}

#[test]
fn a_group_writable_skills_directory_is_unsafe() {
    let rig = Rig::new(&[]);
    let skills = rig.home().join(".claude/skills");
    fs::create_dir(&skills).expect("create the skills directory");
    fs::set_permissions(&skills, fs::Permissions::from_mode(0o770)).expect("group writable");

    let result = rig
        .install(&RuntimeRef::claude(), &HomeSelection::Runtime, true)
        .expect("install");

    assert_eq!(skill_report(&result).found, AgentSkillState::Unsafe);
    assert!(!skills.join("pohunek").exists());
}

#[test]
fn a_marker_without_its_file_reads_stale_and_install_recreates_the_file() {
    let rig = Rig::new(&[]);
    let dir = claude_skill_dir(&rig.home().join(".claude"));
    fs::create_dir_all(&dir).expect("create the skill directory");
    let marker = format!(
        "{{\"schema\":1,\"skill\":\"pohunek\",\"sha256\":\"{}\"}}\n",
        agent_skill_sha256()
    );
    fs::write(dir.join(".pohunek-managed"), marker).expect("write the marker");
    fs::set_permissions(
        dir.join(".pohunek-managed"),
        fs::Permissions::from_mode(FILE_MODE),
    )
    .expect("marker mode");

    assert_eq!(
        skill_status(&rig.statuses(&RuntimeRef::claude(), false)[0]).state,
        AgentSkillState::Stale
    );
    let result = rig
        .install(&RuntimeRef::claude(), &HomeSelection::Runtime, true)
        .expect("install");

    assert_eq!(skill_report(&result).state, AgentSkillChange::Refreshed);
    assert_eq!(
        fs::read_to_string(dir.join("SKILL.md")).expect("read"),
        AGENT_SKILL
    );
}

#[test]
fn a_failed_removal_of_an_owned_skill_is_an_error_not_a_foreign_skill() {
    let rig = Rig::new(&[]);
    let dir = claude_skill_dir(&rig.home().join(".claude"));
    rig.install(&RuntimeRef::claude(), &HomeSelection::Runtime, true)
        .expect("install");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).expect("read-only directory");

    let failure = uninstall_in_with(
        &rig.homes,
        &RuntimeRef::claude(),
        &HomeSelection::Runtime,
        true,
    )
    .expect_err("the removal fails");

    assert_eq!(failure.code, "integration_agent_skill_unsafe");
    fs::set_permissions(&dir, fs::Permissions::from_mode(DIR_MODE)).expect("restore");
    assert!(dir.join("SKILL.md").exists());
}

/// Every file below `root`, to compare a home before and after a refusal.
fn files_below(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("read a directory") {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                pending.push(path.clone());
            }
            found.push(path);
        }
    }
    found.sort();
    found
}

#[test]
fn a_runtime_with_a_handler_and_no_skill_location_refuses_the_skill_before_any_write() {
    let rig = Rig::with_host(&[], cline_shaped_host(Path::new("/bin/sh"), &[]));
    let cline = RuntimeRef::from_wire("cline");
    let config = rig.home().join(".cline");
    fs::create_dir_all(&config).expect("create the config home");
    let before = files_below(&rig.home());

    let refusal = rig
        .install(&cline, &HomeSelection::Runtime, true)
        .expect_err("the skill is refused");

    assert_eq!(refusal.code, "integration_agent_skill_unsupported");
    assert_eq!(files_below(&rig.home()), before, "nothing was written");
    assert!(!config.join("skills").exists());

    let hooks = rig
        .install(&cline, &HomeSelection::Runtime, false)
        .expect("the hooks install without the skill");
    assert!(hooks.installed[0].skill.is_none(), "no skill report");

    let status = rig.statuses(&cline, false);
    let skill = status[0].skill.as_ref().expect("a skill report");
    assert_eq!(skill.state, AgentSkillState::Unsupported);
    assert!(skill.path.is_none());

    let removal = uninstall_in_with(&rig.homes, &cline, &HomeSelection::Runtime, true)
        .expect_err("skill-only removal is refused");
    assert_eq!(removal.code, "integration_agent_skill_unsupported");

    // A request that names no runtime skips the runtime without a skill
    // location and still writes the skill of a runtime that declares one.
    let mixed = Rig::with_host(
        &[],
        cline_shaped_and_official_host(Path::new("/bin/sh"), &[]),
    );
    fs::create_dir_all(mixed.home().join(".cline")).expect("create the Cline config home");
    let installed = install_in_with(
        &mixed.homes,
        None,
        &HomeSelection::Runtime,
        &RetainedSchemas::default(),
        true,
    )
    .expect("an unnamed request installs every served home it finds");
    let report_of = |agent: &str| {
        installed
            .installed
            .iter()
            .find(|home| home.agent.as_wire() == agent)
            .unwrap_or_else(|| panic!("{agent} is installed"))
    };
    let cline_home = report_of("cline");
    assert!(
        !cline_home.hook_path.is_empty() && Path::new(&cline_home.hook_path).exists(),
        "the hooks of a runtime without a skill location are installed"
    );
    assert!(
        cline_home.skill.is_none(),
        "a runtime without a skill location is skipped"
    );
    let claude_home = report_of("claude");
    assert_eq!(
        claude_home.skill.as_ref().expect("a skill report").state,
        AgentSkillChange::Created
    );
    assert_eq!(
        fs::read_to_string(claude_skill_dir(&mixed.home().join(".claude")).join("SKILL.md"))
            .expect("read the skill"),
        AGENT_SKILL
    );
}

#[test]
fn a_runtime_without_a_daemon_handler_answers_the_skill_with_the_unsupported_code() {
    let rig = Rig::new(&[]);
    let pi = RuntimeRef::from_wire("pi");

    let install = rig
        .install(&pi, &HomeSelection::Runtime, true)
        .expect_err("the skill is refused");
    assert_eq!(install.code, "integration_agent_skill_unsupported");
    assert!(
        install
            .msg
            .contains("pi has no daemon-managed hook integration"),
        "{}",
        install.msg
    );
    assert_eq!(
        install.recover.as_deref(),
        Some("drop --skill: the daemon installs an agent skill only beside hooks it manages")
    );

    let removal = uninstall_in_with(&rig.homes, &pi, &HomeSelection::Runtime, true)
        .expect_err("skill-only removal is refused");
    assert_eq!(removal.code, "integration_agent_skill_unsupported");

    let hooks = rig
        .install(&pi, &HomeSelection::Runtime, false)
        .expect_err("the hooks stay unsupported");
    assert_eq!(hooks.code, "agent_not_installable");

    // A profile over the same runtime answers the same refusal, named after
    // the profile, and so does a request that names the runtime as well.
    let profiled = Rig::with_host(
        &[(
            "pi-work",
            format!("base = \"pi\"\n{}", pi_shaped_profile_pin()),
        )],
        pi_shaped_host(Path::new("/bin/sh"), PI_SHAPED_NO_CHECK),
    );
    let profile = HomeSelection::Profile("pi-work".to_owned());
    let by_profile = install_in_with(
        &profiled.homes,
        None,
        &profile,
        &RetainedSchemas::default(),
        true,
    )
    .expect_err("the skill is refused");
    assert_eq!(by_profile.code, "integration_agent_skill_unsupported");
    assert!(
        by_profile
            .msg
            .contains("profile pi-work has no daemon-managed hook integration"),
        "{}",
        by_profile.msg
    );

    let profile_removal = uninstall_in_with(&profiled.homes, &pi, &profile, true)
        .expect_err("skill-only removal is refused");
    assert_eq!(profile_removal.code, "integration_agent_skill_unsupported");
    assert!(
        profile_removal
            .msg
            .contains("profile pi-work has no daemon-managed hook integration"),
        "{}",
        profile_removal.msg
    );

    let named = install_in_with(
        &profiled.homes,
        Some(&pi),
        &profile,
        &RetainedSchemas::default(),
        true,
    )
    .expect_err("the skill is refused");
    assert_eq!(named.code, "integration_agent_skill_unsupported");
}
