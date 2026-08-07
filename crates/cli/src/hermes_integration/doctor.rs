//! Structured, payload-free diagnostics for the managed Hermes plugin.
//!
//! Doctor deliberately inspects only the selected plugin, external policy, and
//! fixed local runner. It neither reads Hermes state databases nor contacts an
//! allowed remote host.

// Rust guideline compliant 2026-08-07

use std::fs::{self, File, OpenOptions};
use std::io::Read as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use nix::unistd::Uid;
use serde::Serialize;

use super::lifecycle;
use super::policy::{Policy, WildcardConfirmation};
use super::runner::{HermesRunner, InstalledProbe};
use super::target::ResolvedTarget;

/// A policy is small configuration; this cap rejects replacement-file abuse.
const MAX_POLICY_BYTES: usize = 64 * 1024;
/// The installed probe accepts only the seven contract-defined lifecycle hooks.
const MANAGED_HOOK_COUNT: u8 = 7;
/// Hooks must finish inside the one-second bound enforced by the Python probe.
const HOOK_LATENCY_CEILING_MS: u32 = 1_000;
/// The complete, stable report inventory in user-facing presentation order.
const CHECK_CODES: [&str; 15] = [
    "hermes_executable",
    "hermes_version",
    "target_safety",
    "plugin_ownership",
    "asset_integrity",
    "plugin_enabled",
    "policy_schema_permissions",
    "pohunek_cli_compatibility",
    "tool_registration",
    "skill_registration",
    "hook_registration",
    "host_allowlist_syntax",
    "access_mode",
    "stale_stage",
    "stale_backup",
];

/// One deterministic doctor result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Report {
    /// Whether every required inventory entry passed.
    pub(crate) ok: bool,
    /// Fixed-order checks with payload-free recovery hints.
    pub(crate) checks: Vec<Check>,
}

/// One payload-free diagnostic check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Check {
    /// Stable machine-readable check identifier.
    pub(crate) code: &'static str,
    /// Deterministic check state.
    pub(crate) status: Status,
    /// Stable operator recovery hint with no paths or subprocess content.
    pub(crate) recovery_hint: &'static str,
}

/// The outcome state for one doctor check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    /// The check completed and met its complete contract.
    Pass,
    /// The check completed but found an operator-repairable failure.
    Fail,
    /// A prerequisite failed, so this check intentionally did not execute.
    NotRun,
}

impl Check {
    fn pass(code: &'static str) -> Self {
        Self {
            code,
            status: Status::Pass,
            recovery_hint: "none",
        }
    }

    fn fail(code: &'static str, recovery_hint: &'static str) -> Self {
        Self {
            code,
            status: Status::Fail,
            recovery_hint,
        }
    }

    fn not_run(code: &'static str) -> Self {
        Self {
            code,
            status: Status::NotRun,
            recovery_hint: "repair prerequisite checks before retrying",
        }
    }
}

/// Inspects one already-resolved Hermes plugin installation.
///
/// The runner constructor has already validated the executable. This function
/// returns every check even when an earlier check fails, and never exposes
/// selected paths, policy bytes, sockets, or child-process output.
#[must_use]
pub(crate) fn inspect(
    runner: &mut HermesRunner,
    target: &ResolvedTarget,
    policy_path: &Path,
) -> Report {
    let mut checks: Vec<Check> = CHECK_CODES.iter().copied().map(Check::not_run).collect();
    set_pass(&mut checks, "hermes_executable");

    let target_safe = target_is_safe(target, policy_path);
    set_result(
        &mut checks,
        "target_safety",
        target_safe,
        "select an explicit owner-private Hermes target",
    );
    if target_safe {
        set_result(
            &mut checks,
            "hermes_version",
            runner.verify_version(target).is_ok(),
            "install the pinned supported Hermes release",
        );
    }

    let policy = read_policy(policy_path);
    set_result(
        &mut checks,
        "policy_schema_permissions",
        policy.is_some(),
        "repair the owner-private Pohunek policy and its schema",
    );
    if policy.is_some() {
        // `Policy::from_json` validates both fields before returning a policy.
        set_pass(&mut checks, "host_allowlist_syntax");
        set_pass(&mut checks, "access_mode");
    }

    let lifecycle = target_safe.then(|| lifecycle::inspect(runner, target, policy_path));
    match lifecycle {
        Some(Ok(state)) => {
            set_result(
                &mut checks,
                "plugin_ownership",
                state.installed,
                "install the Pohunek-managed plugin for this target",
            );
            set_result(
                &mut checks,
                "asset_integrity",
                state.installed && !state.modified,
                "restore the embedded managed plugin assets",
            );
            set_result(
                &mut checks,
                "plugin_enabled",
                state.enabled,
                "enable the managed Pohunek plugin through Hermes",
            );
            set_result(
                &mut checks,
                "stale_stage",
                !state.stale_stage,
                "preserve and recover the managed staging state",
            );
            set_result(
                &mut checks,
                "stale_backup",
                !state.stale_backup,
                "preserve and recover the managed backup state",
            );
        }
        Some(Err(_)) => {
            set_fail(
                &mut checks,
                "plugin_ownership",
                "repair the managed plugin ownership marker",
            );
        }
        None => {}
    }

    let ready_for_probe = is_pass(&checks, "hermes_version")
        && is_pass(&checks, "plugin_ownership")
        && is_pass(&checks, "asset_integrity")
        && is_pass(&checks, "policy_schema_permissions");
    if ready_for_probe {
        match runner.probe_installed(target, policy_path) {
            Ok(probe) => apply_probe(&mut checks, probe),
            Err(_) => {
                set_fail(
                    &mut checks,
                    "pohunek_cli_compatibility",
                    "repair the fixed Pohunek CLI compatibility",
                );
            }
        }
    }

    let ok = checks.iter().all(|check| check.status == Status::Pass);
    Report { ok, checks }
}

fn target_is_safe(target: &ResolvedTarget, policy_path: &Path) -> bool {
    target.hermes_home().is_absolute()
        && target.plugin_root().is_absolute()
        && target.plugin_root().starts_with(target.hermes_home())
        && policy_path.is_absolute()
        && !policy_path.starts_with(target.hermes_home())
        && !policy_path.starts_with(target.plugin_root())
}

fn read_policy(policy_path: &Path) -> Option<Policy> {
    let file = open_private_policy(policy_path)?;
    let mut bytes = Vec::with_capacity(MAX_POLICY_BYTES);
    file.take((MAX_POLICY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_POLICY_BYTES {
        return None;
    }
    // A wildcard in an already written policy was explicitly confirmed at
    // install time. Doctor validates that stored syntax only; it never scans it.
    Policy::from_json(&bytes, WildcardConfirmation::new(true)).ok()
}

fn open_private_policy(policy_path: &Path) -> Option<File> {
    if !policy_path.is_absolute() || !private_policy_ancestors(policy_path) {
        return None;
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(policy_path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    (metadata.is_file()
        && metadata.uid() == Uid::effective().as_raw()
        && metadata.permissions().mode() & 0o777 == 0o600)
        .then_some(file)
}

fn private_policy_ancestors(policy_path: &Path) -> bool {
    let uid = Uid::effective().as_raw();
    let Ok(root_metadata) = fs::metadata(Path::new("/")) else {
        return false;
    };
    let root_uid = root_metadata.uid();
    let mut current = PathBuf::new();
    for component in policy_path.components() {
        current.push(component.as_os_str());
        let Ok(metadata) = fs::symlink_metadata(&current) else {
            return false;
        };
        if metadata.file_type().is_symlink() {
            return false;
        }
    }
    let Some(parent) = policy_path.parent() else {
        return false;
    };
    for ancestor in parent.ancestors() {
        let Ok(metadata) = fs::metadata(ancestor) else {
            return false;
        };
        let mode = metadata.permissions().mode();
        let root_sticky_shared_directory = metadata.is_dir()
            && metadata.uid() == root_uid
            && mode & 0o1000 != 0
            && mode & 0o022 != 0;
        if !metadata.is_dir()
            || (metadata.uid() != uid && metadata.uid() != root_uid)
            || (mode & 0o022 != 0 && !root_sticky_shared_directory)
        {
            return false;
        }
    }
    true
}

fn apply_probe(checks: &mut [Check], probe: InstalledProbe) {
    let complete = probe.probe_complete && probe.failure_phase == 0 && probe.failure_errno == 0;
    if !complete {
        let (code, recovery_hint) = match probe.failure_phase {
            1 | 3 => (
                "asset_integrity",
                "restore the embedded managed plugin assets",
            ),
            2 | 5 | 21..=214 => (
                "hook_registration",
                "repair the managed hook registration and local reporting",
            ),
            _ => (
                "pohunek_cli_compatibility",
                "repair the fixed Pohunek CLI compatibility",
            ),
        };
        set_fail(checks, code, recovery_hint);
        return;
    }
    set_result(
        checks,
        "pohunek_cli_compatibility",
        probe.integration_ready,
        "repair the fixed Pohunek CLI compatibility",
    );
    set_result(
        checks,
        "tool_registration",
        probe.tools_ok && probe.tool_count > 0,
        "repair the access-mode tool registration",
    );
    set_result(
        checks,
        "skill_registration",
        probe.skill_ok && probe.skill_count == 1,
        "restore the generated bundled skill",
    );
    set_result(
        checks,
        "hook_registration",
        probe.hooks_ok
            && probe.hook_count == MANAGED_HOOK_COUNT
            && probe.hook_no_subprocess
            && probe.hook_no_network
            && probe.hook_no_database
            && probe.forced_socket_failure_swallowed
            && probe.hook_latency_ms <= HOOK_LATENCY_CEILING_MS,
        "repair the managed hook registration and local reporting",
    );
}

fn set_result(checks: &mut [Check], code: &str, passed: bool, recovery_hint: &'static str) {
    if passed {
        set_pass(checks, code);
    } else {
        set_fail(checks, code, recovery_hint);
    }
}

fn set_pass(checks: &mut [Check], code: &str) {
    if let Some(check) = checks.iter_mut().find(|check| check.code == code) {
        *check = Check::pass(check.code);
    }
}

fn set_fail(checks: &mut [Check], code: &str, recovery_hint: &'static str) {
    if let Some(check) = checks.iter_mut().find(|check| check.code == code) {
        *check = Check::fail(check.code, recovery_hint);
    }
}

fn is_pass(checks: &[Check], code: &str) -> bool {
    checks
        .iter()
        .find(|check| check.code == code)
        .is_some_and(|check| check.status == Status::Pass)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{symlink, PermissionsExt as _};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::hermes_integration::assets;
    use crate::hermes_integration::target::{ProfileName, TargetContext, TargetSelection};

    const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
    const PRIVATE_FILE_MODE: u32 = 0o600;
    const SUPPORTED_VERSION: &str = "Hermes Agent v0.20.0 (2026.8.3)";
    static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

    struct Fixture {
        root: PathBuf,
        target: ResolvedTarget,
        runner: HermesRunner,
        policy: PathBuf,
        cli: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.root) {
                assert_eq!(
                    error.kind(),
                    std::io::ErrorKind::NotFound,
                    "cleanup fixture"
                );
            }
        }
    }

    fn fixture(tag: &str, plugin_status: &str, access_mode: &str) -> Fixture {
        let root = unique_private_directory(tag);
        let target = resolved_target(&root);
        let config = root.join("config");
        create_private_directory(&config);
        let cli = write_executable(
            &root.join("pohunek"),
            "printf '%s\\n' '{\"protocol\":{\"minimum\":2,\"maximum\":2},\"ok\":{}}'",
        );
        let policy = config.join("policy.json");
        write_policy(&policy, &cli, access_mode);
        write_plugin_tree(target.plugin_root(), target.hermes_home(), &policy);

        let installation = root.join("installation");
        let venv_bin = installation.join("venv/bin");
        let runtime = installation.join("python/bin/python3");
        create_private_directory_tree(&root, &venv_bin);
        create_private_directory_tree(&root, runtime.parent().expect("runtime parent"));
        write_executable(&runtime, "exec /usr/bin/python3 \"$@\"");
        symlink("../../python/bin/python3", venv_bin.join("python3"))
            .expect("private internal runtime link");
        let hermes_body = format!(
            "case \"${{1-}}\" in\n  --version) printf '%s\\n' '{SUPPORTED_VERSION}' ;;\n  plugins) printf '%s\\n' '[{{\"name\":\"pohunek\",\"status\":\"{plugin_status}\",\"source\":\"user\"}}]' ;;\n  *) exit 1 ;;\nesac"
        );
        let hermes = write_executable(&venv_bin.join("hermes"), &hermes_body);
        let runner = HermesRunner::new(&hermes).expect("controlled Hermes runner");
        Fixture {
            root,
            target,
            runner,
            policy,
            cli,
        }
    }

    fn unique_private_directory(tag: &str) -> PathBuf {
        loop {
            let path = std::env::temp_dir().join(format!(
                "pohunek-hermes-doctor-{tag}-{}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => {
                    set_mode(&path, PRIVATE_DIRECTORY_MODE);
                    return path;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("create fixture: {error}"),
            }
        }
    }

    fn resolved_target(root: &Path) -> ResolvedTarget {
        let hermes = root.join("hermes-home");
        let home = root.join("home");
        let workspace = root.join("workspace");
        for path in [&hermes, &home, &workspace] {
            create_private_directory(path);
        }
        TargetContext::new(hermes, home, vec![workspace])
            .expect("target context")
            .resolve(TargetSelection::Profile(ProfileName::default()))
            .expect("target")
    }

    fn create_private_directory(path: &Path) {
        fs::create_dir_all(path).expect("create private directory");
        set_mode(path, PRIVATE_DIRECTORY_MODE);
    }

    fn create_private_directory_tree(root: &Path, path: &Path) {
        let relative = path
            .strip_prefix(root)
            .expect("private contained directory");
        let mut current = root.to_owned();
        for component in relative.components() {
            current.push(component.as_os_str());
            if !current.exists() {
                fs::create_dir(&current).expect("create private tree component");
            }
            set_mode(&current, PRIVATE_DIRECTORY_MODE);
        }
    }

    fn write_executable(path: &Path, body: &str) -> PathBuf {
        fs::write(path, format!("#!/bin/sh\nset -eu\n{body}\n")).expect("write executable");
        set_mode(path, PRIVATE_DIRECTORY_MODE);
        path.to_owned()
    }

    fn write_policy(policy: &Path, cli: &Path, access_mode: &str) {
        let document = serde_json::json!({
            "schema_version": 1,
            "pohunek_cli": cli,
            "protocol_min": 2,
            "protocol_max": 2,
            "access_mode": access_mode,
            "allowed_hosts": ["local"],
            "tool_timeout_ms": 1_000,
            "max_output_bytes": 65_536,
            "max_screen_bytes": 32_768,
            "max_concurrency": 2,
        });
        fs::write(policy, serde_json::to_vec(&document).expect("policy JSON"))
            .expect("write policy");
        set_mode(policy, PRIVATE_FILE_MODE);
    }

    fn write_plugin_tree(root: &Path, hermes_home: &Path, policy: &Path) {
        create_private_directory(root);
        let rendered = assets::render(policy).expect("render plugin assets");
        for asset in &rendered {
            let destination = root.join(asset.path());
            create_private_directory_tree(root, destination.parent().expect("asset parent"));
            fs::write(&destination, asset.bytes()).expect("write managed asset");
            set_mode(&destination, PRIVATE_FILE_MODE);
        }
        let ownership = assets::ownership(hermes_home, policy, &rendered).expect("ownership");
        let marker = root.join(assets::MARKER_NAME);
        fs::write(
            &marker,
            assets::marker_bytes(&ownership).expect("ownership marker"),
        )
        .expect("write ownership marker");
        set_mode(&marker, PRIVATE_FILE_MODE);
    }

    fn set_mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set private mode");
    }

    fn check(report: &Report, code: &str) -> Status {
        report
            .checks
            .iter()
            .find(|check| check.code == code)
            .expect("stable check")
            .status
    }

    fn assert_no_fixture_payload(report: &Report, fixture: &Fixture) {
        let document = serde_json::to_string(report).expect("report JSON");
        assert!(!document.contains(fixture.root.to_str().expect("fixture UTF-8")));
        assert!(!document.contains("sentinel-private-payload"));
        assert!(!document.contains("stdout"));
        assert!(!document.contains("stderr"));
    }

    #[test]
    fn clean_full_install_passes_the_complete_inventory() {
        let mut fixture = fixture("clean", "enabled", "full");
        let report = inspect(&mut fixture.runner, &fixture.target, &fixture.policy);
        assert!(report.ok);
        assert_eq!(report.checks.len(), CHECK_CODES.len());
        assert!(
            report
                .checks
                .iter()
                .all(|check| check.status == Status::Pass),
            "clean installation must pass every stable check"
        );
        assert_no_fixture_payload(&report, &fixture);
    }

    #[test]
    fn missing_install_keeps_inventory_and_skips_probe() {
        let mut fixture = fixture("missing", "enabled", "full");
        fs::remove_dir_all(fixture.target.plugin_root()).expect("remove managed plugin");
        let report = inspect(&mut fixture.runner, &fixture.target, &fixture.policy);
        assert!(!report.ok);
        assert_eq!(report.checks.len(), CHECK_CODES.len());
        for code in ["plugin_ownership", "asset_integrity", "plugin_enabled"] {
            assert_eq!(check(&report, code), Status::Fail, "{code}");
        }
        for code in [
            "pohunek_cli_compatibility",
            "tool_registration",
            "skill_registration",
            "hook_registration",
        ] {
            assert_eq!(check(&report, code), Status::NotRun, "{code}");
        }
        assert_no_fixture_payload(&report, &fixture);
    }

    #[test]
    fn modified_assets_and_invalid_markers_fail_closed_without_probe() {
        let mut modified = fixture("modified", "enabled", "full");
        let asset = modified.target.plugin_root().join("tools.py");
        fs::write(&asset, b"sentinel-private-payload").expect("modify managed asset");
        set_mode(&asset, PRIVATE_FILE_MODE);
        let report = inspect(&mut modified.runner, &modified.target, &modified.policy);
        assert_eq!(check(&report, "plugin_ownership"), Status::Pass);
        assert_eq!(check(&report, "asset_integrity"), Status::Fail);
        assert_eq!(check(&report, "tool_registration"), Status::NotRun);
        assert_no_fixture_payload(&report, &modified);

        let mut invalid_marker = fixture("marker", "enabled", "full");
        let marker = invalid_marker
            .target
            .plugin_root()
            .join(assets::MARKER_NAME);
        fs::write(&marker, b"{\"unknown\":\"sentinel-private-payload\"}")
            .expect("invalidate ownership marker");
        set_mode(&marker, PRIVATE_FILE_MODE);
        let report = inspect(
            &mut invalid_marker.runner,
            &invalid_marker.target,
            &invalid_marker.policy,
        );
        assert_eq!(check(&report, "plugin_ownership"), Status::Fail);
        assert_eq!(check(&report, "asset_integrity"), Status::NotRun);
        assert_eq!(check(&report, "hook_registration"), Status::NotRun);
        assert_no_fixture_payload(&report, &invalid_marker);
    }

    #[test]
    fn unsafe_policy_parent_is_rejected_without_running_dependents() {
        let mut fixture = fixture("policy-parent", "enabled", "full");
        set_mode(
            fixture.policy.parent().expect("policy parent"),
            PRIVATE_DIRECTORY_MODE | 0o077,
        );
        let report = inspect(&mut fixture.runner, &fixture.target, &fixture.policy);
        assert_eq!(check(&report, "policy_schema_permissions"), Status::Fail);
        assert_eq!(check(&report, "host_allowlist_syntax"), Status::NotRun);
        assert_eq!(check(&report, "access_mode"), Status::NotRun);
        assert_eq!(check(&report, "tool_registration"), Status::NotRun);
        assert_no_fixture_payload(&report, &fixture);
    }

    #[test]
    fn disabled_plugin_and_exact_stale_siblings_are_reported() {
        let mut fixture = fixture("stale", "disabled", "full");
        let plugin_parent = fixture
            .target
            .plugin_root()
            .parent()
            .expect("plugin parent");
        // These prefixes are the lifecycle's deliberately exact managed sibling contract.
        create_private_directory(&plugin_parent.join(".pohunek-stage-doctor"));
        write_plugin_tree(
            &plugin_parent.join(".pohunek-backup-doctor"),
            fixture.target.hermes_home(),
            &fixture.policy,
        );
        let unrelated = plugin_parent.join("unrelated-pohunek-stage");
        create_private_directory(&unrelated);

        let report = inspect(&mut fixture.runner, &fixture.target, &fixture.policy);
        assert_eq!(check(&report, "plugin_enabled"), Status::Fail);
        assert_eq!(check(&report, "stale_stage"), Status::Fail);
        assert_eq!(check(&report, "stale_backup"), Status::Fail);
        assert_eq!(check(&report, "tool_registration"), Status::Pass);
        assert_no_fixture_payload(&report, &fixture);
    }

    #[test]
    fn invalid_policy_forms_fail_without_disclosing_policy_content() {
        enum Mutation {
            UnsafeMode,
            Symlink,
            Oversize,
            Malformed,
            InvalidHost,
            InvalidAccessMode,
            InvalidProtocol,
            MissingCli,
        }
        for mutation in [
            Mutation::UnsafeMode,
            Mutation::Symlink,
            Mutation::Oversize,
            Mutation::Malformed,
            Mutation::InvalidHost,
            Mutation::InvalidAccessMode,
            Mutation::InvalidProtocol,
            Mutation::MissingCli,
        ] {
            let mut fixture = fixture("policy", "enabled", "full");
            match mutation {
                Mutation::UnsafeMode => set_mode(&fixture.policy, 0o644),
                Mutation::Symlink => {
                    let actual = fixture.root.join("policy-actual.json");
                    fs::rename(&fixture.policy, &actual).expect("move policy for link");
                    symlink(&actual, &fixture.policy).expect("policy symlink");
                }
                Mutation::Oversize => {
                    fs::write(&fixture.policy, vec![b'x'; MAX_POLICY_BYTES + 1])
                        .expect("oversize policy");
                    set_mode(&fixture.policy, PRIVATE_FILE_MODE);
                }
                Mutation::Malformed => {
                    fs::write(&fixture.policy, b"{sentinel-private-payload")
                        .expect("malformed policy");
                    set_mode(&fixture.policy, PRIVATE_FILE_MODE);
                }
                Mutation::InvalidHost => {
                    rewrite_policy(&fixture.policy, &fixture.cli, "full", -1, 2, "invalid host");
                }
                Mutation::InvalidAccessMode => {
                    rewrite_policy(&fixture.policy, &fixture.cli, "unsafe", 2, 2, "local");
                }
                Mutation::InvalidProtocol => {
                    rewrite_policy(&fixture.policy, &fixture.cli, "full", 3, 2, "local");
                }
                Mutation::MissingCli => rewrite_policy(
                    &fixture.policy,
                    &fixture.root.join("missing-cli"),
                    "full",
                    2,
                    2,
                    "local",
                ),
            }
            let report = inspect(&mut fixture.runner, &fixture.target, &fixture.policy);
            assert_eq!(check(&report, "policy_schema_permissions"), Status::Fail);
            assert_eq!(check(&report, "host_allowlist_syntax"), Status::NotRun);
            assert_eq!(check(&report, "access_mode"), Status::NotRun);
            assert_eq!(check(&report, "hook_registration"), Status::NotRun);
            assert_no_fixture_payload(&report, &fixture);
        }
    }

    #[test]
    fn access_modes_and_registration_failures_are_checked_at_runtime() {
        for (mode, expected_tools) in [("read_only", 7), ("manage", 14), ("full", 16)] {
            let mut fixture = fixture("access", "enabled", mode);
            let probe = fixture
                .runner
                .probe_installed(&fixture.target, &fixture.policy)
                .expect("complete mode probe");
            assert!(probe.probe_complete, "{mode}");
            assert_eq!(probe.tool_count, expected_tools, "{mode}");
            assert!(
                !fixture.target.plugin_root().join("__pycache__").exists(),
                "probe must not modify the managed installation"
            );
            let report = inspect(&mut fixture.runner, &fixture.target, &fixture.policy);
            assert_eq!(check(&report, "tool_registration"), Status::Pass, "{mode}");
            assert_eq!(check(&report, "skill_registration"), Status::Pass, "{mode}");
            assert_eq!(check(&report, "hook_registration"), Status::Pass, "{mode}");
        }

        let mut missing_skill = fixture("missing-skill", "enabled", "full");
        fs::remove_file(
            missing_skill
                .target
                .plugin_root()
                .join("skills/pohunek/SKILL.md"),
        )
        .expect("remove generated skill");
        let report = inspect(
            &mut missing_skill.runner,
            &missing_skill.target,
            &missing_skill.policy,
        );
        assert_eq!(check(&report, "asset_integrity"), Status::Fail);
        assert_eq!(check(&report, "skill_registration"), Status::NotRun);

        let mut incompatible_cli = fixture("bad-cli", "enabled", "full");
        fs::write(&incompatible_cli.cli, "#!/bin/sh\nprintf '%s\\n' '{\"protocol\":{\"minimum\":99,\"maximum\":99},\"ok\":{}}'\n")
            .expect("rewrite controlled CLI");
        set_mode(&incompatible_cli.cli, PRIVATE_DIRECTORY_MODE);
        let report = inspect(
            &mut incompatible_cli.runner,
            &incompatible_cli.target,
            &incompatible_cli.policy,
        );
        assert_eq!(check(&report, "pohunek_cli_compatibility"), Status::Fail);
        assert_eq!(check(&report, "tool_registration"), Status::Fail);
        assert_eq!(check(&report, "skill_registration"), Status::Fail);
        assert_eq!(check(&report, "hook_registration"), Status::Pass);
    }

    fn rewrite_policy(
        policy: &Path,
        cli: &Path,
        access_mode: &str,
        protocol_min: i32,
        protocol_max: i32,
        host: &str,
    ) {
        let document = serde_json::json!({
            "schema_version": 1,
            "pohunek_cli": cli,
            "protocol_min": protocol_min,
            "protocol_max": protocol_max,
            "access_mode": access_mode,
            "allowed_hosts": [host],
            "tool_timeout_ms": 1_000,
            "max_output_bytes": 65_536,
            "max_screen_bytes": 32_768,
            "max_concurrency": 2,
        });
        fs::write(
            policy,
            serde_json::to_vec(&document).expect("mutated policy JSON"),
        )
        .expect("rewrite policy");
        set_mode(policy, PRIVATE_FILE_MODE);
    }

    fn complete_probe() -> InstalledProbe {
        InstalledProbe {
            probe_complete: true,
            failure_phase: 0,
            failure_errno: 0,
            tool_count: 7,
            tools_ok: true,
            skill_count: 1,
            skill_ok: true,
            hook_count: MANAGED_HOOK_COUNT,
            hooks_ok: true,
            integration_ready: true,
            hook_no_subprocess: true,
            hook_no_network: true,
            hook_no_database: true,
            forced_socket_failure_swallowed: true,
            hook_latency_ms: HOOK_LATENCY_CEILING_MS,
        }
    }

    #[test]
    fn report_inventory_is_stable_and_incomplete_reports_fail() {
        let checks: Vec<_> = CHECK_CODES.iter().copied().map(Check::not_run).collect();
        let report = Report { ok: false, checks };
        assert_eq!(report.checks.len(), CHECK_CODES.len());
        assert_eq!(report.checks[0].code, "hermes_executable");
        assert_eq!(report.checks[14].code, "stale_backup");
        assert!(!report.ok);
    }

    #[test]
    fn report_json_carries_no_dynamic_diagnostic_payload() {
        let check = Check::fail(
            "asset_integrity",
            "restore the embedded managed plugin assets",
        );
        let document = serde_json::to_string(&check).expect("serialize check");
        assert!(!document.contains('/'));
        assert!(!document.contains("stdout"));
        assert!(!document.contains("stderr"));
    }

    #[test]
    fn complete_probe_passes_each_registration_check() {
        let mut checks: Vec<_> = CHECK_CODES.iter().copied().map(Check::not_run).collect();
        apply_probe(&mut checks, complete_probe());
        for code in [
            "pohunek_cli_compatibility",
            "tool_registration",
            "skill_registration",
            "hook_registration",
        ] {
            assert!(is_pass(&checks, code), "{code} should pass");
        }
    }

    #[test]
    fn complete_probe_cli_failure_does_not_hide_independent_hook_success() {
        let mut probe = complete_probe();
        probe.integration_ready = false;
        let mut checks: Vec<_> = CHECK_CODES.iter().copied().map(Check::not_run).collect();
        apply_probe(&mut checks, probe);
        assert_eq!(
            checks
                .iter()
                .find(|check| check.code == "pohunek_cli_compatibility")
                .expect("CLI check")
                .status,
            Status::Fail
        );
        for code in [
            "tool_registration",
            "skill_registration",
            "hook_registration",
        ] {
            assert!(is_pass(&checks, code), "{code} stays independently passed");
        }
    }

    #[test]
    fn incomplete_probe_fails_only_the_phase_nearest_check() {
        let mut probe = complete_probe();
        probe.probe_complete = false;
        probe.failure_phase = 211;
        probe.failure_errno = 1;
        let mut checks: Vec<_> = CHECK_CODES.iter().copied().map(Check::not_run).collect();
        apply_probe(&mut checks, probe);
        assert_eq!(
            check(
                &Report {
                    ok: false,
                    checks: checks.clone()
                },
                "hook_registration"
            ),
            Status::Fail
        );
        for code in [
            "pohunek_cli_compatibility",
            "tool_registration",
            "skill_registration",
        ] {
            assert_eq!(
                checks
                    .iter()
                    .find(|check| check.code == code)
                    .expect("registered check")
                    .status,
                Status::NotRun
            );
        }
    }

    #[test]
    fn incomplete_probe_phases_preserve_independent_registration_checks() {
        for (phase, failed) in [
            (1, "asset_integrity"),
            (2, "hook_registration"),
            (3, "asset_integrity"),
            (4, "pohunek_cli_compatibility"),
            (5, "hook_registration"),
        ] {
            let mut probe = complete_probe();
            probe.probe_complete = false;
            probe.failure_phase = phase;
            let mut checks: Vec<_> = CHECK_CODES.iter().copied().map(Check::not_run).collect();
            apply_probe(&mut checks, probe);
            assert_eq!(
                checks
                    .iter()
                    .find(|check| check.code == failed)
                    .expect("phase check")
                    .status,
                Status::Fail,
                "phase {phase}"
            );
            for check in checks.iter().filter(|check| {
                [
                    "pohunek_cli_compatibility",
                    "tool_registration",
                    "skill_registration",
                    "hook_registration",
                ]
                .contains(&check.code)
                    && check.code != failed
            }) {
                assert_eq!(
                    check.status,
                    Status::NotRun,
                    "phase {phase}: {}",
                    check.code
                );
            }
        }
    }
}
