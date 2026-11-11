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
    IntegrationInstallResult, IntegrationStatusParams, IntegrationUninstallResult, ProtocolError,
    RuntimeRef,
};

use super::{ConfigHomes, HomeSelection};
use crate::agent::host::fixture::{official_host, official_profile_pin};
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
        let registry = ProfileRegistry::with_runtimes(Some(agents), official_host());
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
