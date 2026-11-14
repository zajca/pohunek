//! `pohunek plugin profile rm` (#2346) against a real daemon.
//!
//! The `pohunek` binary runs against the harness's in-process daemon, so the
//! file the daemon removes or leaves is the file the assertions inspect, and
//! a session started from a profile is a real launch. Every digest the
//! commands pass is the `file_digest` that `plugin profile list --json --all`
//! reports, as the onboarding Undo records it.

// Rust guideline compliant 2026-11-14

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use serde_json::Value;

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::{path_str, stderr_text, stdout_text, Harness};

/// A profile on the built-in `shell` whose session stays alive.
const SLEEPING_PROFILE: &str = "base = \"shell\"\nargs = [\"-c\", \"exec sleep 30\"]\n";

/// A parent profile with one option, for presets.
const PARENT_PROFILE: &str = "base = \"shell\"\n\
     [options.mode]\nlabel = \"Mode\"\ndefault = \"calm\"\n\
     choices.calm = { label = \"Calm\", args = [\"-c\", \"exec sleep 30\"] }\n\
     choices.bold = { label = \"Bold\", args = [\"-c\", \"exec sleep 31\"] }\n";

/// A preset of `work` that selects `bold`.
const PRESET_OF_WORK: &str = "[preset]\nparent = \"work\"\noptions = { mode = \"bold\" }\n";

/// A digest no profile file in these tests has.
fn other_digest() -> String {
    format!("sha256:{}", "0".repeat(64))
}

/// The `file_digest` that `plugin profile list --json --all` reports for
/// `name`, or `None` when the name is not listed.
async fn listed_digest(harness: &Harness, name: &str) -> Option<String> {
    let (code, document) = harness.json(&["plugin", "profile", "list", "--all"]).await;
    assert_eq!(code, 0, "{document}");
    document["ok"]["profiles"]
        .as_array()
        .expect("profiles")
        .iter()
        .find(|entry| entry["name"] == name)
        .map(|entry| {
            entry["file_digest"]
                .as_str()
                .unwrap_or_else(|| panic!("{name} has a file digest: {entry}"))
                .to_owned()
        })
}

/// `plugin profile rm <name> --digest <digest> --json`.
async fn rm(harness: &Harness, name: &str, digest: &str) -> (i32, Value) {
    harness
        .json(&["plugin", "profile", "rm", name, "--digest", digest])
        .await
}

/// Bytes, mode and inode of the file at `path`.
fn fingerprint(path: &Path) -> (Vec<u8>, u32, u64) {
    let metadata = fs::symlink_metadata(path).expect("the file exists");
    (
        fs::read(path).expect("read the file"),
        metadata.mode(),
        metadata.ino(),
    )
}

/// Rows 1 and 11: the current digest removes the file, and `--json` reports
/// exactly the removed name and digest; the listing no longer shows it.
#[tokio::test]
async fn rm_with_the_current_digest_removes_the_profile() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let harness = Harness::start().await;
    let path = harness.profile("work", SLEEPING_PROFILE);
    let digest = listed_digest(&harness, "work")
        .await
        .expect("work is listed");

    let (code, removed) = rm(&harness, "work", &digest).await;

    assert_eq!(code, 0, "{removed}");
    assert_eq!(
        removed["ok"],
        serde_json::json!({ "profile": "work", "file_digest": digest })
    );
    assert!(!path.exists(), "the profile file is gone");
    assert_eq!(listed_digest(&harness, "work").await, None);
    let leftovers: Vec<String> = fs::read_dir(&harness.agents)
        .expect("list the agents directory")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert!(leftovers.is_empty(), "no temporary remains: {leftovers:?}");

    // The text form names the profile and the digest.
    harness.profile("work", SLEEPING_PROFILE);
    let output = harness
        .run(&["plugin", "profile", "rm", "work", "--digest", &digest])
        .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr_text(&output));
    assert_eq!(
        stdout_text(&output),
        format!("removed work digest {digest}\n")
    );
    harness.stop().await;
}

/// Row 2: a digest that is not the file's leaves the file exactly as found.
#[tokio::test]
async fn rm_with_another_digest_leaves_the_file_as_found() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let harness = Harness::start().await;
    let path = harness.profile("work", SLEEPING_PROFILE);
    let before = fingerprint(&path);

    let (code, refused) = rm(&harness, "work", &other_digest()).await;

    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "package_profile_digest_mismatch");
    assert_eq!(fingerprint(&path), before);
    harness.stop().await;
}

/// Rows 3 and 5: the built-in `shell` and a name with neither a file nor a
/// runtime are refused by kind, and nothing is created or removed.
#[tokio::test]
async fn rm_of_a_runtime_or_an_unknown_name_is_refused_by_kind() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let harness = Harness::start().await;
    let path = harness.profile("work", SLEEPING_PROFILE);
    let before = fingerprint(&path);

    for (name, code) in [
        ("shell", "package_profile_builtin"),
        ("ghost", "package_profile_not_found"),
    ] {
        let (exit, refused) = rm(&harness, name, &other_digest()).await;
        assert_eq!(exit, 1, "{name}: {refused}");
        assert_eq!(refused["err"]["code"], code, "{name}: {refused}");
    }
    assert_eq!(fingerprint(&path), before);
    harness.stop().await;
}

/// Row 6: a profile name that is a symbolic link, or a hard-linked file, is
/// refused, and neither the link nor its target changes.
#[tokio::test]
async fn rm_of_a_linked_profile_is_refused_and_nothing_changes() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let harness = Harness::start().await;
    let target = harness.profile("work", SLEEPING_PROFILE);
    let digest = listed_digest(&harness, "work")
        .await
        .expect("work is listed");
    let target_before = fingerprint(&target);
    let symlink = harness.agents.join("alias.toml");
    std::os::unix::fs::symlink(&target, &symlink).expect("symlink the profile");
    let hard = harness.agents.join("twin.toml");
    fs::hard_link(&target, &hard).expect("hard-link the profile");

    for name in ["alias", "twin"] {
        let (code, refused) = rm(&harness, name, &digest).await;
        assert_eq!(code, 1, "{name}: {refused}");
        assert_eq!(
            refused["err"]["code"], "package_profile_unusable",
            "{name}: {refused}"
        );
    }
    assert!(fs::symlink_metadata(&symlink)
        .expect("the link remains")
        .file_type()
        .is_symlink());
    assert!(hard.exists(), "the hard link remains");
    assert_eq!(fingerprint(&target), target_before);
    harness.stop().await;
}

/// Row 7: a profile that a running session uses is refused and stays; once
/// the session is stopped the same command removes it.
#[tokio::test]
async fn rm_of_a_profile_a_running_session_uses_is_refused() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let harness = Harness::start().await;
    let path = harness.profile("work", SLEEPING_PROFILE);
    let digest = listed_digest(&harness, "work")
        .await
        .expect("work is listed");
    let cwd = harness.env.cwd().to_path_buf();
    let (code, session) = harness
        .json(&["session", "new", "--agent", "work", "--cwd", path_str(&cwd)])
        .await;
    assert_eq!(code, 0, "{session}");
    let id = session["ok"]["id"].as_str().expect("session id").to_owned();
    let before = fingerprint(&path);

    let (code, refused) = rm(&harness, "work", &digest).await;

    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "package_profile_in_use");
    assert_eq!(fingerprint(&path), before);

    let (code, stopped) = harness.json(&["session", "stop", &id]).await;
    assert_eq!(code, 0, "{stopped}");
    let (code, removed) = rm(&harness, "work", &digest).await;
    assert_eq!(code, 0, "{removed}");
    assert!(!path.exists());
    harness.stop().await;
}

/// Rows 9 and 10: a parent that a preset names is refused until the preset
/// is removed; a preset is removable even when its parent file is gone.
#[tokio::test]
async fn rm_of_a_parent_waits_for_its_presets() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let harness = Harness::start().await;
    let parent = harness.profile("work", PARENT_PROFILE);
    let preset = harness.profile("work-bold", PRESET_OF_WORK);
    let parent_digest = listed_digest(&harness, "work")
        .await
        .expect("work is listed");
    let preset_digest = listed_digest(&harness, "work-bold")
        .await
        .expect("the preset is listed");
    let before = fingerprint(&parent);

    let (code, refused) = rm(&harness, "work", &parent_digest).await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "package_profile_has_presets");
    assert_eq!(fingerprint(&parent), before);

    let (code, removed) = rm(&harness, "work-bold", &preset_digest).await;
    assert_eq!(code, 0, "{removed}");
    assert!(!preset.exists());
    let (code, removed) = rm(&harness, "work", &parent_digest).await;
    assert_eq!(code, 0, "{removed}");
    assert!(!parent.exists());

    // A preset whose parent file is gone is still removable.
    let orphan = harness.profile("orphan", PRESET_OF_WORK);
    let orphan_digest = listed_digest(&harness, "orphan")
        .await
        .expect("the orphan preset is listed");
    let (code, removed) = rm(&harness, "orphan", &orphan_digest).await;
    assert_eq!(code, 0, "{removed}");
    assert!(!orphan.exists());
    harness.stop().await;
}

/// Rows 13 and 15: a remote host is refused before any connection, and a
/// missing `--digest` is a usage error; neither touches the file.
#[tokio::test]
async fn rm_without_a_digest_or_on_a_remote_host_sends_nothing() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let harness = Harness::start().await;
    let path = harness.profile("work", SLEEPING_PROFILE);
    let digest = listed_digest(&harness, "work")
        .await
        .expect("work is listed");
    let before = fingerprint(&path);

    let (code, refused) = harness
        .json(&[
            "--host", "other", "plugin", "profile", "rm", "work", "--digest", &digest,
        ])
        .await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "plugin_local_only");

    let output = harness.run(&["plugin", "profile", "rm", "work"]).await;
    assert_eq!(output.status.code(), Some(2), "{}", stderr_text(&output));
    assert!(
        stderr_text(&output).contains("--digest"),
        "{}",
        stderr_text(&output)
    );

    assert_eq!(fingerprint(&path), before);
    harness.stop().await;
}

/// Review r1 F1: a running session holds the profile when its agent name
/// opens the same file, not only when it equals the name. `WORK.toml` links
/// to `work.toml` in the tree, which is the lookup a case-insensitive volume
/// makes for a session started as `--agent WORK`; the removal of `work` is
/// refused and the file stays.
#[tokio::test]
async fn rm_is_refused_while_a_session_runs_the_profile_under_another_name() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let harness = Harness::start().await;
    let path = harness.profile("work", SLEEPING_PROFILE);
    std::os::unix::fs::symlink("work.toml", harness.agents.join("WORK.toml"))
        .expect("link the other spelling");
    let digest = listed_digest(&harness, "work")
        .await
        .expect("work is listed");
    let cwd = harness.env.cwd().to_path_buf();
    let (code, session) = harness
        .json(&["session", "new", "--agent", "WORK", "--cwd", path_str(&cwd)])
        .await;
    assert_eq!(code, 0, "{session}");
    let before = fingerprint(&path);

    let (code, refused) = rm(&harness, "work", &digest).await;

    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "package_profile_in_use");
    assert_eq!(fingerprint(&path), before);
    harness.stop().await;
}

/// Review r1 F1: a preset holds its parent when its `parent` opens the same
/// file through an in-tree link; the removal of the parent is refused.
#[tokio::test]
async fn rm_is_refused_while_a_preset_names_the_parent_through_a_link() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let harness = Harness::start().await;
    let parent = harness.profile("work", PARENT_PROFILE);
    std::os::unix::fs::symlink("work.toml", harness.agents.join("alias.toml"))
        .expect("link the alias");
    harness.profile(
        "alias-bold",
        "[preset]\nparent = \"alias\"\noptions = { mode = \"bold\" }\n",
    );
    let digest = listed_digest(&harness, "work")
        .await
        .expect("work is listed");
    let before = fingerprint(&parent);

    let (code, refused) = rm(&harness, "work", &digest).await;

    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "package_profile_has_presets");
    assert_eq!(fingerprint(&parent), before);
    harness.stop().await;
}

/// Review r1 F2: a removal quarantine entry that an interrupted removal left
/// in the agents directory is swept by the next removal; another platform
/// quarantine, which may hold an edited profile, is kept.
#[tokio::test]
async fn rm_sweeps_a_removal_quarantine_left_by_an_interrupted_removal() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let harness = Harness::start().await;
    let path = harness.profile("work", SLEEPING_PROFILE);
    let digest = listed_digest(&harness, "work")
        .await
        .expect("work is listed");
    let leftover = harness.agents.join(".pohunek-remove-0123456789abcdef");
    fs::write(&leftover, "base = \"shell\"\n").expect("plant the quarantine");
    let parked = harness.agents.join(".pohunek-restore-0123456789abcdef");
    fs::write(&parked, "base = \"shell\"\n").expect("plant the parked entry");

    let (code, removed) = rm(&harness, "work", &digest).await;

    assert_eq!(code, 0, "{removed}");
    assert!(!path.exists());
    assert!(!leftover.exists(), "the removal quarantine is swept");
    assert!(parked.exists(), "a parked entry is never swept");
    harness.stop().await;
}

/// Review r1 F1: on a case-insensitive volume `Work` opens `work.toml`; a
/// removal under another spelling than the stored name is refused and the
/// file stays. On a case-sensitive volume `Work` names no file at all, so the
/// expectation is the same.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn rm_under_another_spelling_than_the_stored_name_is_refused() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let harness = Harness::start().await;
    let path = harness.profile("work", SLEEPING_PROFILE);
    let digest = listed_digest(&harness, "work")
        .await
        .expect("work is listed");
    let before = fingerprint(&path);

    let (code, refused) = rm(&harness, "Work", &digest).await;

    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "package_profile_not_found");
    assert_eq!(fingerprint(&path), before);
    harness.stop().await;
}
