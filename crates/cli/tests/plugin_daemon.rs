//! End-to-end contracts of `pohunek plugin` against a real daemon.
//!
//! The `pohunek` binary runs as a subprocess against a `ControlServer` served
//! from this test process over a real `SessionRegistry` and a real plugin root,
//! so every assertion exercises the daemon's package lifecycle, not a scripted
//! reply. `plugin_cli.rs` covers the binary's request shapes against a scripted
//! server; this file covers what the daemon does with them. Registry state is
//! also read straight from the plugin root through `package::registry`.

// Rust guideline compliant 2026-10-04

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use package::{build_archive, read_archive, ArchiveEntry, Limits};
use serde_json::Value;

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::{path_str, stderr_text, stdout_text, Built, Harness, PROFILE_MODE};

/// Package id of the fixture archive.
const PACKAGE_ID: &str = "acme.runtime.pi";

/// Runtime id the fixture package serves.
const RUNTIME: &str = "pi";

/// Programs the fixture runtime launches; it only has to exist.
const PROGRAM: &str = "/bin/sh";

const DETECT_MANIFEST: &str = r#"[[rules]]
id = "idle_prompt"
state = "idle"
priority = 100
region = "whole_recent"
any = [{ contains = "ready" }]
"#;

impl Harness {
    /// Writes a package archive of `version` that serves [`RUNTIME`].
    fn archive(&self, version: &str) -> Built {
        let document = runtime_document(version);
        let entries = [
            ArchiveEntry {
                path: "runtime.toml".to_owned(),
                contents: document.into_bytes(),
                executable: false,
            },
            ArchiveEntry {
                path: "detect.toml".to_owned(),
                contents: DETECT_MANIFEST.as_bytes().to_vec(),
                executable: false,
            },
        ];
        let bytes = build_archive(&entries, &Limits::DEFAULT).expect("the archive builds");
        let digest = read_archive(&bytes, &Limits::DEFAULT)
            .expect("the archive reads")
            .digest()
            .clone();
        let path = self.env.root().join(format!("pi-{version}.tar.zst"));
        fs::write(&path, bytes).expect("write the archive");
        Built { path, digest }
    }

    /// Writes a developer directory of `version` for `plugin link`.
    fn directory(&self, version: &str) -> PathBuf {
        let dir = self.env.root().join(format!("pi-dev-{version}"));
        fs::create_dir_all(&dir).expect("create the package directory");
        fs::write(dir.join("runtime.toml"), runtime_document(version)).expect("write runtime");
        fs::write(dir.join("detect.toml"), DETECT_MANIFEST).expect("write detect manifest");
        dir
    }
}

fn runtime_document(version: &str) -> String {
    format!(
        r#"schema = 1
id = "{PACKAGE_ID}"
version = "{version}"
runtime_api = 1

[runtime]
id = "{RUNTIME}"
name = "Pi"
program = "{PROGRAM}"
args = ["--model", "fast"]
detect_manifest = "detect.toml"
prompt_arg = true

[input]
bracketed_paste = false
submit_delay_ms = 0
text_policy = "unrestricted"

[resume]
supported = true
reference_kind = "id"
args = ["--session", "{{reference}}"]

[fork]
supported = true
args = ["--fork", "{{reference}}"]

[native_reference]
strategy = "assigned"
launch_args = ["--session-id", "{{reference}}"]

[native_reference.existence]
check = "none"
"#
    )
}

/// Whether the host advertises `agent` among the runtimes a fresh session can
/// launch, read through `pohunek host inspect`.
async fn serves_agent(harness: &Harness, agent: &str) -> bool {
    let (code, document) = harness.json(&["host", "inspect", "local"]).await;
    assert_eq!(code, 0, "{document}");
    document["ok"]["supported_agents"]
        .as_array()
        .expect("supported_agents")
        .iter()
        .any(|entry| entry == agent)
}

/// Installs the `1.0.0` archive enabled and selected through the CLI.
async fn install_enabled(harness: &Harness, built: &Built) {
    let (code, document) = harness
        .json(&[
            "plugin",
            "install",
            path_str(&built.path),
            "--sha256",
            built.digest.as_str(),
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{document}");
    assert_eq!(document["ok"]["status"], "installed", "{document}");
}

/// Starts a session of `agent` in the hermetic working directory.
async fn launch(harness: &Harness, agent: &str) -> (i32, Value) {
    let cwd = harness.env.cwd().to_path_buf();
    harness
        .json(&["session", "new", "--agent", agent, "--cwd", path_str(&cwd)])
        .await
}

#[tokio::test]
async fn install_needs_consent_and_the_exact_digest() {
    let harness = Harness::start().await;
    let built = harness.archive("1.0.0");
    let wrong = format!("sha256:{}", "0".repeat(64));

    // Without --yes the daemon previews and nothing is recorded.
    let output = harness
        .run(&[
            "plugin",
            "install",
            path_str(&built.path),
            "--sha256",
            built.digest.as_str(),
        ])
        .await;
    assert_eq!(output.status.code(), Some(1), "{}", stderr_text(&output));
    let stdout = stdout_text(&output);
    for needle in [PACKAGE_ID, "1.0.0", PROGRAM, built.digest.as_str()] {
        assert!(stdout.contains(needle), "missing {needle:?}: {stdout}");
    }
    assert!(
        stderr_text(&output).contains("needs your consent; nothing was changed"),
        "{}",
        stderr_text(&output)
    );
    let (code, document) = harness
        .json(&[
            "plugin",
            "install",
            path_str(&built.path),
            "--sha256",
            built.digest.as_str(),
        ])
        .await;
    assert_eq!(code, 1);
    assert_eq!(document["err"]["code"], "consent_required");
    assert!(harness.registry().packages().is_empty());
    assert!(!serves_agent(&harness, RUNTIME).await);

    // A digest that is not the archive's is refused even with consent.
    let (code, document) = harness
        .json(&[
            "plugin",
            "install",
            path_str(&built.path),
            "--sha256",
            &wrong,
            "--yes",
        ])
        .await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(
        document["err"]["code"], "package_archive_invalid",
        "{document}"
    );
    assert!(harness.registry().packages().is_empty());
    assert!(!serves_agent(&harness, RUNTIME).await);

    // The exact digest with consent installs it enabled and selected.
    install_enabled(&harness, &built).await;
    let state = harness.registry();
    let [record] = state.packages() else {
        panic!("one package is installed: {:?}", state.packages());
    };
    assert_eq!(record.digest(), &built.digest);
    assert!(record.enabled());
    assert_eq!(
        state.selected(&PACKAGE_ID.parse().expect("package id")),
        Some(&built.digest)
    );
    assert!(serves_agent(&harness, RUNTIME).await);

    harness.stop().await;
}

#[tokio::test]
async fn list_and_inspect_show_the_installed_package() {
    let harness = Harness::start().await;
    let built = harness.archive("1.0.0");
    install_enabled(&harness, &built).await;

    let (code, listed) = harness.json(&["plugin", "list"]).await;
    assert_eq!(code, 0, "{listed}");
    let packages = listed["ok"]["packages"].as_array().expect("packages");
    assert_eq!(packages.len(), 1);
    assert_eq!(packages[0]["digest"], built.digest.as_str());
    assert_eq!(packages[0]["enabled"], true);
    assert_eq!(packages[0]["selected"], true);

    let (code, inspected) = harness.json(&["plugin", "inspect", PACKAGE_ID]).await;
    assert_eq!(code, 0, "{inspected}");
    assert_eq!(inspected["ok"]["runtime"]["program"], PROGRAM);
    assert_eq!(inspected["ok"]["package"]["digest"], built.digest.as_str());

    let table = harness.run(&["plugin", "list"]).await;
    assert!(table.status.success(), "{}", stderr_text(&table));
    assert!(stdout_text(&table).contains(PACKAGE_ID));

    harness.stop().await;
}

#[tokio::test]
async fn disable_blocks_a_fresh_launch_and_enable_restores_it() {
    let harness = Harness::start().await;
    let built = harness.archive("1.0.0");
    install_enabled(&harness, &built).await;
    let (code, launched) = launch(&harness, RUNTIME).await;
    assert_eq!(code, 0, "an enabled package launches: {launched}");
    assert!(launched["ok"]["id"].is_string(), "{launched}");

    let (code, disabled) = harness.json(&["plugin", "disable", PACKAGE_ID]).await;
    assert_eq!(code, 0, "{disabled}");
    assert!(!harness.registry().packages()[0].enabled());
    assert!(!serves_agent(&harness, RUNTIME).await);
    let (code, refused) = launch(&harness, RUNTIME).await;
    assert_eq!(code, 1, "a disabled package is not launchable: {refused}");
    assert_eq!(
        refused["err"]["code"], "agent_profile_not_found",
        "{refused}"
    );

    let (code, enabled) = harness.json(&["plugin", "enable", PACKAGE_ID]).await;
    assert_eq!(code, 0, "{enabled}");
    assert!(harness.registry().packages()[0].enabled());
    assert!(serves_agent(&harness, RUNTIME).await);
    let (code, relaunched) = launch(&harness, RUNTIME).await;
    assert_eq!(code, 0, "enable restores the launch: {relaunched}");

    harness.stop().await;
}

#[tokio::test]
async fn update_installs_side_by_side_and_select_rolls_back() {
    let harness = Harness::start().await;
    let old = harness.archive("1.0.0");
    let new = harness.archive("1.1.0");
    install_enabled(&harness, &old).await;

    let (code, updated) = harness
        .json(&[
            "plugin",
            "update",
            PACKAGE_ID,
            path_str(&new.path),
            "--sha256",
            new.digest.as_str(),
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{updated}");
    let id = PACKAGE_ID.parse().expect("package id");
    let state = harness.registry();
    assert_eq!(state.packages().len(), 2, "both versions stay installed");
    assert!(state.package(&old.digest).is_some());
    assert_eq!(state.selected(&id), Some(&new.digest));

    let (code, rolled_back) = harness
        .json(&[
            "plugin",
            "select",
            PACKAGE_ID,
            "--digest",
            &old.digest.as_str()[7..19],
        ])
        .await;
    assert_eq!(code, 0, "{rolled_back}");
    assert_eq!(harness.registry().selected(&id), Some(&old.digest));

    // A bare id is now ambiguous only for mutations that need one version;
    // inspect prefers the selected version.
    let (code, inspected) = harness.json(&["plugin", "inspect", PACKAGE_ID]).await;
    assert_eq!(code, 0, "{inspected}");
    assert_eq!(inspected["ok"]["package"]["digest"], old.digest.as_str());

    harness.stop().await;
}

#[tokio::test]
async fn link_installs_disabled_and_enable_serves_the_runtime() {
    let harness = Harness::start().await;
    let dir = harness.directory("0.1.0");

    let without_consent = harness.run(&["plugin", "link", path_str(&dir)]).await;
    assert_eq!(without_consent.status.code(), Some(1));
    assert!(harness.registry().packages().is_empty());

    let linked = harness
        .run(&["plugin", "link", path_str(&dir), "--yes"])
        .await;
    assert_eq!(linked.status.code(), Some(0), "{}", stderr_text(&linked));
    let state = harness.registry();
    let [record] = state.packages() else {
        panic!("one package is linked: {:?}", state.packages());
    };
    assert!(!record.enabled(), "a linked package starts disabled");
    assert!(state.selected(&record.identity().id).is_none());
    assert!(!serves_agent(&harness, RUNTIME).await);

    // Follow exactly the commands the output tells the owner to run.
    let output = stdout_text(&linked);
    let instructions: Vec<Vec<String>> = output
        .lines()
        .filter_map(|line| {
            let start = line.find("`pohunek ")? + "`pohunek ".len();
            let end = line[start..].find('`')? + start;
            Some(
                line[start..end]
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect(),
            )
        })
        .collect();
    assert!(
        instructions.iter().any(|words| words[1] == "enable"),
        "the output says how to enable it: {output}"
    );
    assert!(
        instructions.iter().any(|words| words[1] == "select"),
        "the output says how to select it: {output}"
    );
    for words in &instructions {
        let arguments: Vec<&str> = words.iter().map(String::as_str).collect();
        let result = harness.run(&arguments).await;
        assert_eq!(result.status.code(), Some(0), "{}", stderr_text(&result));
    }
    assert!(
        serves_agent(&harness, RUNTIME).await,
        "the runtime is served once the displayed steps ran"
    );

    harness.stop().await;
}

#[tokio::test]
async fn a_modified_root_fails_doctor_and_needs_remove_modified() {
    let harness = Harness::start().await;
    let built = harness.archive("1.0.0");
    install_enabled(&harness, &built).await;

    let clean = harness.run(&["plugin", "doctor"]).await;
    assert!(clean.status.success(), "{}", stderr_text(&clean));

    // The installed tree is read-only; a user with write access can still
    // change it, which is what verification must catch.
    let tampered = harness.root_of(&built.digest).join("files/runtime.toml");
    fs::set_permissions(&tampered, fs::Permissions::from_mode(PROFILE_MODE))
        .expect("make the file writable");
    fs::write(&tampered, "tampered").expect("tamper with the installed tree");

    let (code, doctor) = harness.json(&["plugin", "doctor"]).await;
    assert_eq!(code, 1, "{doctor}");
    assert_eq!(doctor["ok"]["findings"][0]["fault"], "root_modified");

    let (code, refused) = harness
        .json(&["plugin", "uninstall", PACKAGE_ID, "--yes"])
        .await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "package_root_invalid");
    assert!(harness.registry().package(&built.digest).is_some());

    let (code, removed) = harness
        .json(&[
            "plugin",
            "uninstall",
            PACKAGE_ID,
            "--remove-modified",
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{removed}");
    assert!(harness.registry().packages().is_empty());
    assert!(!harness.root_of(&built.digest).exists());

    let after = harness.run(&["plugin", "doctor"]).await;
    assert!(after.status.success(), "{}", stderr_text(&after));

    harness.stop().await;
}

#[tokio::test]
async fn a_pinned_profile_blocks_uninstall_and_migrate_pins_the_new_digest() {
    let harness = Harness::start().await;
    let old = harness.archive("1.0.0");
    let new = harness.archive("1.1.0");
    install_enabled(&harness, &old).await;

    let pinned = format!(
        "base = \"{RUNTIME}\"\npackage = \"{PACKAGE_ID}\"\ndigest = \"{}\"\n",
        old.digest
    );
    let path = harness.profile("p", &pinned);

    let (code, refused) = harness
        .json(&["plugin", "uninstall", PACKAGE_ID, "--yes"])
        .await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "package_referenced");
    assert!(harness.registry().package(&old.digest).is_some());

    // A newer version leaves the profile on the digest it pinned.
    let (code, updated) = harness
        .json(&[
            "plugin",
            "update",
            PACKAGE_ID,
            path_str(&new.path),
            "--sha256",
            new.digest.as_str(),
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{updated}");
    assert_eq!(fs::read_to_string(&path).expect("read"), pinned);
    let (code, listed) = harness.json(&["plugin", "profile", "list"]).await;
    assert_eq!(code, 0, "{listed}");
    assert_eq!(listed["ok"]["profiles"][0]["state"], "pinned");
    assert_eq!(listed["ok"]["profiles"][0]["digest"], old.digest.as_str());

    let (code, migrated) = harness
        .json(&[
            "plugin",
            "profile",
            "migrate",
            "p",
            "--digest",
            &new.digest.as_str()[7..19],
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{migrated}");
    assert_eq!(
        fs::read_to_string(&path).expect("read"),
        pinned.replace(old.digest.as_str(), new.digest.as_str())
    );
    let (code, listed) = harness.json(&["plugin", "profile", "list"]).await;
    assert_eq!(code, 0, "{listed}");
    assert_eq!(listed["ok"]["profiles"][0]["state"], "pinned");
    assert_eq!(listed["ok"]["profiles"][0]["digest"], new.digest.as_str());

    // The daemon accepts the migrated profile for a fresh launch.
    let (code, launched) = launch(&harness, "p").await;
    assert_eq!(code, 0, "{launched}");

    // The old version is no longer pinned and uninstalls.
    let (code, removed) = harness
        .json(&[
            "plugin",
            "uninstall",
            PACKAGE_ID,
            "--digest",
            &old.digest.as_str()[7..19],
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{removed}");
    assert!(harness.registry().package(&old.digest).is_none());

    harness.stop().await;
}

#[tokio::test]
async fn an_unpinned_profile_migrates_to_the_installed_package() {
    let harness = Harness::start().await;
    let built = harness.archive("1.0.0");
    install_enabled(&harness, &built).await;
    let path = harness.profile("work", &format!("base = \"{RUNTIME}\"\n"));

    let (code, listed) = harness.json(&["plugin", "profile", "list"]).await;
    assert_eq!(code, 0, "{listed}");
    assert_eq!(listed["ok"]["profiles"][0]["state"], "needs_migration");

    let (code, migrated) = harness
        .json(&["plugin", "profile", "migrate", "work", "--yes"])
        .await;
    assert_eq!(code, 0, "{migrated}");
    assert_eq!(migrated["ok"]["status"], "bound");
    let text = fs::read_to_string(&path).expect("read the profile");
    assert!(text.contains(built.digest.as_str()), "{text}");
    let (code, listed) = harness.json(&["plugin", "profile", "list"]).await;
    assert_eq!(code, 0, "{listed}");
    assert_eq!(listed["ok"]["profiles"][0]["state"], "pinned");

    harness.stop().await;
}

#[tokio::test]
async fn profile_migrate_maps_the_daemon_refusals_and_leaves_profiles_untouched() {
    let harness = Harness::start().await;
    let built = harness.archive("1.0.0");
    install_enabled(&harness, &built).await;
    let real = harness.profile("real", &format!("base = \"{RUNTIME}\"\n"));
    std::os::unix::fs::symlink(&real, harness.agents.join("link.toml")).expect("symlink");
    let loose = harness.profile("loose", &format!("base = \"{RUNTIME}\"\n"));
    fs::set_permissions(&loose, fs::Permissions::from_mode(0o660)).expect("loosen");
    harness.profile("broken", "base = \"pi\"\n[env]\nTOKEN = oops\n");
    harness.profile("shell", "base = \"shell\"\n");
    let cases = [
        ("link", "package_profile_unusable"),
        ("loose", "package_profile_unusable"),
        ("broken", "package_profile_unusable"),
        ("shell", "package_profile_base_builtin"),
        ("absent", "package_profile_not_found"),
    ];
    for (name, code) in cases {
        let (exit, document) = harness
            .json(&["plugin", "profile", "migrate", name, "--yes"])
            .await;
        assert_eq!(exit, 1, "{name}: {document}");
        assert_eq!(document["err"]["code"], code, "{name}");
    }
    let (exit, document) = harness
        .json(&[
            "plugin",
            "profile",
            "migrate",
            "real",
            "--digest",
            &format!("sha256:{}", "0".repeat(64)),
            "--yes",
        ])
        .await;
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["err"]["code"], "package_profile_target_invalid");
    assert_eq!(
        fs::read_to_string(&real).expect("read"),
        format!("base = \"{RUNTIME}\"\n")
    );

    harness.stop().await;
}

#[tokio::test]
async fn a_remote_host_is_refused_without_touching_the_daemon() {
    let harness = Harness::start().await;
    let built = harness.archive("1.0.0");
    install_enabled(&harness, &built).await;
    let before = harness.registry();

    let output = harness
        .run(&["--host", "elsewhere", "plugin", "list", "--json"])
        .await;
    assert_eq!(output.status.code(), Some(1), "{}", stderr_text(&output));
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(document["err"]["code"], "plugin_local_only");

    let output = harness
        .run(&["--host", "elsewhere", "plugin", "disable", PACKAGE_ID])
        .await;
    assert_eq!(output.status.code(), Some(1));
    let after = harness.registry();
    assert_eq!(after.generation(), before.generation());
    assert!(after.packages()[0].enabled());

    harness.stop().await;
}

#[tokio::test]
async fn update_to_a_recorded_version_enables_and_selects_it() {
    let harness = Harness::start().await;
    let old = harness.archive("1.0.0");
    let new = harness.archive("1.1.0");
    install_enabled(&harness, &old).await;
    // A second version installed with `install` stays disabled and unselected.
    let (code, installed) = harness
        .json(&[
            "plugin",
            "install",
            path_str(&new.path),
            "--sha256",
            new.digest.as_str(),
            "--no-enable",
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{installed}");
    let id = PACKAGE_ID.parse().expect("package id");
    let state = harness.registry();
    assert!(!state.package(&new.digest).expect("recorded").enabled());
    assert_eq!(state.selected(&id), Some(&old.digest));

    let (code, updated) = harness
        .json(&[
            "plugin",
            "update",
            PACKAGE_ID,
            path_str(&new.path),
            "--sha256",
            new.digest.as_str(),
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{updated}");
    assert_eq!(updated["ok"]["status"], "already_installed");
    assert_eq!(updated["ok"]["package"]["enabled"], true, "{updated}");
    assert_eq!(updated["ok"]["package"]["selected"], true, "{updated}");
    let state = harness.registry();
    assert!(state.package(&new.digest).expect("recorded").enabled());
    assert_eq!(state.selected(&id), Some(&new.digest));
    assert!(
        state.package(&old.digest).is_some(),
        "the old version stays"
    );

    harness.stop().await;
}
