//! The assistant package state `pohunek service upgrade` reports: the
//! upgrade never installs the package, so it tells the owner when the selected
//! version is older than the core and which command moves it.
//!
//! A real `pohunekd` serves the installation, the real `pohunek` binary
//! installs and updates the package by digest, and the report is read from the
//! daemon the way the upgrade reads it after the swap.

// Rust guideline compliant 2026-11-21

#![cfg(unix)]

use std::fs;
use std::path::Path;

use package::{build_archive, read_archive, ArchiveEntry, Limits};
use pohunek_cli::service::report::{AcceptanceGroups, AssistantPackageStatus, UpgradeReport};
use pohunek_cli::service::VERSION;
use pohunek_cli::upgrade_assistant;
use pohunek_paths::{BasePaths, PathEnv, Platform};

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::{path_str, Harness};

/// Package id of the assistant context package.
const ASSISTANT_PACKAGE: &str = "pohunek.context.assistant";

/// A version older than every core this build can be.
const OLDER_VERSION: &str = "0.0.1";

/// Writes an assistant context package archive of `version` below `dir` and
/// returns its path and bare SHA-256 hex.
fn context_archive(dir: &Path, version: &str) -> (std::path::PathBuf, String) {
    let manifest = format!(
        "schema = 1\nid = \"{ASSISTANT_PACKAGE}\"\nversion = \"{version}\"\nruntime_api = 1\nrequires = [\"package-context-v1\"]\n\n[context]\ndir = \"bundle\"\n"
    );
    let entries = [
        ArchiveEntry {
            path: "runtime.toml".to_owned(),
            contents: manifest.into_bytes(),
            executable: false,
        },
        ArchiveEntry {
            path: "bundle/index.md".to_owned(),
            contents: b"# Index\n".to_vec(),
            executable: false,
        },
    ];
    let bytes = build_archive(&entries, &Limits::DEFAULT).expect("the package archive builds");
    let digest = read_archive(&bytes, &Limits::DEFAULT)
        .expect("the archive reads")
        .digest()
        .clone();
    let path = dir.join(format!("assistant-{version}.tar.zst"));
    fs::write(&path, bytes).expect("write the context archive");
    (
        path,
        digest.as_str().trim_start_matches("sha256:").to_owned(),
    )
}

/// The paths of the installation the harness daemon serves.
fn base_paths(harness: &Harness) -> BasePaths {
    let env = &harness.env;
    BasePaths::resolve_for(
        Platform::current().expect("platform"),
        nix::unistd::Uid::effective().as_raw(),
        &PathEnv {
            xdg_runtime_dir: Some(env.runtime_dir().into()),
            xdg_data_home: Some(env.data_home().into()),
            xdg_state_home: Some(env.state_home().into()),
            xdg_cache_home: Some(env.cache_home().into()),
            xdg_config_home: Some(env.config_home().into()),
            home: Some(env.home().into()),
        },
    )
    .expect("resolve the harness paths")
}

/// An upgrade report to `to_version` with nothing else to tell.
fn upgrade_report(to_version: &str) -> UpgradeReport {
    UpgradeReport {
        warnings: Vec::new(),
        from_version: "0.0.1".to_owned(),
        to_version: to_version.to_owned(),
        unchanged: false,
        resumed: false,
        rolled_back: None,
        removed_versions: Vec::new(),
        kept_versions: Vec::new(),
        gc_error: None,
        preflight: None,
        accepted_runtime_loss: false,
        acceptance: AcceptanceGroups::default(),
        config_schema_migration: None,
        runtimes: None,
        integrations: Vec::new(),
        repair: None,
        refused_entries: Vec::new(),
        assistant: None,
    }
}

/// The upgrade's view of the package follows what the daemon lists: absent,
/// older than the core with the update command, current after the update, and
/// unchecked once the daemon is gone.
#[tokio::test]
async fn the_upgrade_reports_an_older_assistant_package_with_the_update_command() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let harness = Harness::start().await;
    let paths = base_paths(&harness);
    let dir = harness.env.cwd().to_path_buf();

    let absent = upgrade_assistant::state(VERSION, &paths).await;
    assert_eq!(absent.status, AssistantPackageStatus::Absent, "{absent:?}");
    assert_eq!(absent.version, None, "{absent:?}");

    let (older, older_digest) = context_archive(&dir, OLDER_VERSION);
    let (code, installed) = harness
        .json(&[
            "plugin",
            "install",
            path_str(&older),
            "--sha256",
            &older_digest,
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{installed}");
    let outdated = upgrade_assistant::state(VERSION, &paths).await;
    assert_eq!(
        outdated.status,
        AssistantPackageStatus::Outdated,
        "{outdated:?}"
    );
    assert_eq!(outdated.version.as_deref(), Some(OLDER_VERSION));
    assert_eq!(outdated.core_version, VERSION);
    assert_eq!(outdated.command.as_deref(), Some("pohunek setup"));
    assert_eq!(outdated.package_id, ASSISTANT_PACKAGE);
    let text = upgrade_assistant::render(&outdated);
    for needle in [OLDER_VERSION, VERSION, "pohunek setup", ASSISTANT_PACKAGE] {
        assert!(text.contains(needle), "{needle}: {text}");
    }
    let json = serde_json::to_value(&outdated).expect("the report serializes");
    assert_eq!(json["status"], "outdated", "{json}");
    assert_eq!(json["command"], "pohunek setup", "{json}");
    assert_eq!(json["version"], OLDER_VERSION, "{json}");
    assert_eq!(json["core_version"], VERSION, "{json}");

    // The finished upgrade report carries the state in `--json` and prints the
    // line in its text.
    let finished = upgrade_assistant::attach(upgrade_report(VERSION), &paths).await;
    let document = serde_json::to_value(&finished).expect("the upgrade report serializes");
    assert_eq!(document["assistant"]["status"], "outdated", "{document}");
    assert_eq!(document["assistant"]["version"], OLDER_VERSION, "{document}");
    assert_eq!(document["assistant"]["core_version"], VERSION, "{document}");
    let upgrade_text = upgrade_assistant::render_report(&finished);
    assert!(
        upgrade_text.contains("pohunek setup") && upgrade_text.contains(OLDER_VERSION),
        "{upgrade_text}"
    );

    // A selected version that cannot be ordered is never reported as older.
    let (unordered, unordered_digest) = context_archive(&dir, "nightly");
    let (code, updated) = harness
        .json(&[
            "plugin",
            "update",
            ASSISTANT_PACKAGE,
            path_str(&unordered),
            "--sha256",
            &unordered_digest,
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{updated}");
    let state = upgrade_assistant::state(VERSION, &paths).await;
    assert_eq!(state.status, AssistantPackageStatus::Current, "{state:?}");
    assert_eq!(state.version.as_deref(), Some("nightly"), "{state:?}");
    assert_eq!(upgrade_assistant::render(&state), "", "{state:?}");

    let (current, current_digest) = context_archive(&dir, VERSION);
    let (code, updated) = harness
        .json(&[
            "plugin",
            "update",
            ASSISTANT_PACKAGE,
            path_str(&current),
            "--sha256",
            &current_digest,
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{updated}");
    let state = upgrade_assistant::state(VERSION, &paths).await;
    assert_eq!(state.status, AssistantPackageStatus::Current, "{state:?}");
    assert_eq!(upgrade_assistant::render(&state), "", "{state:?}");
    assert_eq!(state.version.as_deref(), Some(VERSION));
    assert_eq!(state.command, None, "{state:?}");

    harness.stop().await;
    let unchecked = upgrade_assistant::state(VERSION, &paths).await;
    assert_eq!(
        unchecked.status,
        AssistantPackageStatus::Unchecked,
        "{unchecked:?}"
    );
    assert!(unchecked.detail.is_some(), "{unchecked:?}");
    let text = upgrade_assistant::render(&unchecked);
    assert!(text.contains("was not checked"), "{text}");
    assert!(text.contains("pohunek plugin list"), "{text}");
}
