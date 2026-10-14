//! End-to-end contracts of `pohunek plugin` against a real daemon.
//!
//! The `pohunek` binary runs as a subprocess against a `ControlServer` served
//! from this test process over a real `SessionRegistry` and a real plugin root,
//! so every assertion exercises the daemon's package lifecycle, not a scripted
//! reply. `plugin_cli.rs` covers the binary's request shapes against a scripted
//! server; this file covers what the daemon does with them. Registry state is
//! also read straight from the plugin root through `package::registry`.

// Rust guideline compliant 2026-10-13

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use package::{build_archive, read_archive, ArchiveEntry, CatalogEntry, Limits};
use pohunek_daemon::catalog_anchor::CatalogTrust;
use pohunek_daemon::session::HostTrustAnchor;
use pohunek_test_support::catalog::{
    attestations_for, catalog_of, host_platform, root_of, signed, test_key, ANY_CORE, RUNTIME_API,
    WINDOW_END, WINDOW_START,
};
use protocol::{PackageId, PackageVersion, RuntimeId};
use serde_json::Value;

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::{path_str, stderr_text, stdout_text, Built, Harness, PROFILE_MODE};

/// Package id of the fixture archive.
const PACKAGE_ID: &str = "acme.runtime.pi";

/// Runtime id the fixture package serves.
const RUNTIME: &str = "pi";

/// Package id of the fixture archive that serves the official `codex` alias
/// through a signed catalog.
const OFFICIAL_CODEX_ID: &str = "pohunek.runtime.codex";

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
        self.archive_of(PACKAGE_ID, RUNTIME, version)
    }

    /// Writes a package archive `package` of `version` that serves `runtime`.
    fn archive_of(&self, package: &str, runtime: &str, version: &str) -> Built {
        let document = runtime_document_of(package, runtime, version);
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
        let path = self.env.root().join(format!("{runtime}-{version}.tar.zst"));
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
    runtime_document_of(PACKAGE_ID, RUNTIME, version)
}

fn runtime_document_of(package: &str, runtime: &str, version: &str) -> String {
    format!(
        r#"schema = 1
id = "{package}"
version = "{version}"
runtime_api = 1

[runtime]
id = "{runtime}"
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
async fn an_unpinned_profile_whose_base_nothing_serves_reports_the_missing_package() {
    let harness = Harness::start().await;
    // No package is installed: only the shell is built into the daemon.
    assert!(serves_agent(&harness, "shell").await);
    assert!(!serves_agent(&harness, "codex").await);
    assert!(!serves_agent(&harness, RUNTIME).await);
    harness.profile("codex-work", "base = \"codex\"\n");
    harness.profile("pi-work", &format!("base = \"{RUNTIME}\"\n"));
    harness.profile("shell-work", "base = \"shell\"\n");

    let (code, listed) = harness.json(&["plugin", "profile", "list"]).await;
    assert_eq!(code, 0, "{listed}");
    let entry = |name: &str| {
        listed["ok"]["profiles"]
            .as_array()
            .expect("profiles")
            .iter()
            .find(|entry| entry["name"] == name)
            .unwrap_or_else(|| panic!("{name} is listed: {listed}"))
            .clone()
    };
    let detail = |entry: &Value| entry["detail"].as_str().unwrap_or_default().to_owned();

    // An official alias names the install from the release archive layout
    // documented in `docs/install.md`, then the pin.
    let codex = entry("codex-work");
    assert_eq!(codex["state"], "package_not_installed", "{codex}");
    for needle in [
        "pohunek plugin install runtime/packages/codex.tar.zst",
        "--catalog runtime/runtime-catalog.json",
        "pohunek plugin profile migrate codex-work",
    ] {
        assert!(
            detail(&codex).contains(needle),
            "missing {needle:?}: {codex}"
        );
    }

    // Any other base has no official package to name.
    let pi = entry("pi-work");
    assert_eq!(pi["state"], "package_not_installed", "{pi}");
    assert!(
        detail(&pi).contains("pohunek plugin profile migrate pi-work"),
        "{pi}"
    );
    assert!(!detail(&pi).contains("runtime/packages"), "{pi}");

    let shell = entry("shell-work");
    assert_eq!(shell["state"], "builtin", "{shell}");
    assert!(shell.get("detail").is_none(), "{shell}");

    // The daemon agrees that neither unserved profile can launch.
    for name in ["codex-work", "pi-work"] {
        let (code, refused) = launch(&harness, name).await;
        assert_eq!(code, 1, "{refused}");
        assert_eq!(refused["err"]["code"], "runtime_not_installed", "{refused}");
    }

    let output = harness.run(&["plugin", "profile", "list"]).await;
    assert!(output.status.success(), "{}", stderr_text(&output));
    let table = stdout_text(&output);
    assert!(
        table.contains("package_not_installed (install the official `codex` package"),
        "{table}"
    );

    harness.stop().await;
}

/// `plugin install <archive> --catalog <catalog> --yes` of `built`, which a
/// catalog signed by `key` lists as the official package `package` serving
/// `runtime`.
async fn install_official(
    harness: &Harness,
    key: &ed25519_dalek::SigningKey,
    built: &Built,
    package: &str,
    runtime: &str,
) -> (i32, Value) {
    let entry = CatalogEntry {
        package_id: PackageId::parse(package).expect("package id"),
        runtime_id: RuntimeId::parse(runtime).expect("runtime id"),
        version: PackageVersion::parse("1.0.0").expect("package version"),
        digest: built.digest.clone(),
        runtime_api: RUNTIME_API,
        platforms: vec![host_platform()],
        attestations: attestations_for(&[host_platform()]),
        core: ANY_CORE.to_owned(),
    };
    let catalog = harness.env.root().join("runtime-catalog.json");
    fs::write(
        &catalog,
        signed(key, catalog_of(1, WINDOW_END, vec![entry])),
    )
    .expect("write the catalog");
    harness
        .json(&[
            "plugin",
            "install",
            path_str(&built.path),
            "--catalog",
            path_str(&catalog),
            "--yes",
        ])
        .await
}

/// Modifies the installed descriptor of `digest`, so the daemon cannot
/// load the package or tell which runtime it serves.
fn tamper(harness: &Harness, digest: &package::PackageDigest) {
    // The installed tree is read-only; a user with write access can still
    // change it.
    let descriptor = harness.root_of(digest).join("files/runtime.toml");
    fs::set_permissions(&descriptor, fs::Permissions::from_mode(PROFILE_MODE))
        .expect("make the descriptor writable");
    fs::write(&descriptor, "tampered").expect("tamper with the installed tree");
}

#[tokio::test]
async fn an_unpinned_profile_whose_package_cannot_load_points_at_doctor() {
    let key = test_key();
    let anchor = HostTrustAnchor::new(vec![root_of(&key, WINDOW_START, WINDOW_END)], Vec::new())
        .expect("the test trust anchor");
    let harness = Harness::start_with_trust(CatalogTrust::Loaded(anchor)).await;
    harness.profile("codex-work", "base = \"codex\"\n");
    harness.profile("pi-work", &format!("base = \"{RUNTIME}\"\n"));
    harness.profile("shell-work", "base = \"shell\"\n");
    let list = || async {
        let (code, listed) = harness.json(&["plugin", "profile", "list"]).await;
        assert_eq!(code, 0, "{listed}");
        listed["ok"]["profiles"]
            .as_array()
            .expect("profiles")
            .clone()
    };
    let entry = |profiles: &[Value], name: &str| {
        profiles
            .iter()
            .find(|entry| entry["name"] == name)
            .unwrap_or_else(|| panic!("{name} is listed: {profiles:?}"))
            .clone()
    };
    let detail = |entry: &Value| entry["detail"].as_str().unwrap_or_default().to_owned();
    let assert_points_at_doctor = |entry: &Value, name: &str| {
        assert_eq!(entry["state"], "package_unavailable", "{entry}");
        for needle in [
            "pohunek plugin doctor",
            &format!("pohunek plugin profile migrate {name}"),
        ] {
            assert!(
                detail(entry).contains(needle),
                "missing {needle:?}: {entry}"
            );
        }
        assert!(!detail(entry).contains("plugin install"), "{entry}");
    };

    // A local package whose descriptor cannot load may serve `pi`, but never
    // the official alias `codex`, which only a catalog-authorized package can
    // claim.
    let local = harness.archive("1.0.0");
    install_enabled(&harness, &local).await;
    tamper(&harness, &local.digest);
    let profiles = list().await;
    assert_points_at_doctor(&entry(&profiles, "pi-work"), "pi-work");
    let codex = entry(&profiles, "codex-work");
    assert_eq!(codex["state"], "package_not_installed", "{codex}");
    assert!(
        detail(&codex).contains("pohunek plugin install runtime/packages/codex.tar.zst"),
        "{codex}"
    );
    assert_eq!(entry(&profiles, "shell-work")["state"], "builtin");

    // The official package serves `codex` until its installed tree changes.
    let official = harness.archive_of(OFFICIAL_CODEX_ID, "codex", "1.0.0");
    let (code, installed) =
        install_official(&harness, &key, &official, OFFICIAL_CODEX_ID, "codex").await;
    assert_eq!(code, 0, "{installed}");
    assert_eq!(
        installed["ok"]["package"]["origin"], "official",
        "{installed}"
    );
    assert_eq!(
        entry(&list().await, "codex-work")["state"],
        "needs_migration"
    );
    tamper(&harness, &official.digest);
    let profiles = list().await;
    assert_points_at_doctor(&entry(&profiles, "codex-work"), "codex-work");
    assert_eq!(entry(&profiles, "shell-work")["state"], "builtin");

    // Installing the same archive again is refused while its root fails
    // verification, so the install hint would be no recovery.
    let (code, refused) =
        install_official(&harness, &key, &official, OFFICIAL_CODEX_ID, "codex").await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "package_root_invalid", "{refused}");

    // The daemon agrees that neither profile can launch, and the migration
    // cannot pin a faulted package, so the repair comes first. The launch
    // refusal depends on whether the runtime registry was rebuilt since the
    // tree changed, so only the refusal itself is asserted.
    for name in ["codex-work", "pi-work"] {
        let (code, refused) = launch(&harness, name).await;
        assert_eq!(code, 1, "{refused}");
        assert!(refused["err"]["code"].is_string(), "{refused}");
        let (code, refused) = harness
            .json(&["plugin", "profile", "migrate", name, "--yes"])
            .await;
        assert_eq!(code, 1, "{refused}");
        assert_eq!(
            refused["err"]["code"], "package_profile_target_invalid",
            "{refused}"
        );
    }

    harness.stop().await;
}

#[tokio::test]
async fn a_pinned_profile_whose_package_cannot_load_points_at_doctor() {
    let harness = Harness::start().await;
    let built = harness.archive("1.0.0");
    install_enabled(&harness, &built).await;
    harness.profile("work", &format!("base = \"{RUNTIME}\"\n"));
    let (code, migrated) = harness
        .json(&["plugin", "profile", "migrate", "work", "--yes"])
        .await;
    assert_eq!(code, 0, "{migrated}");
    assert_eq!(migrated["ok"]["status"], "bound");
    tamper(&harness, &built.digest);
    // A pin whose package id is not the digest's package needs its pin
    // corrected: repairing the package would not make it usable.
    harness.profile(
        "wrong-id",
        &format!(
            "base = \"{RUNTIME}\"\npackage = \"acme.runtime.other\"\ndigest = \"{}\"\n",
            built.digest
        ),
    );

    let (code, listed) = harness.json(&["plugin", "profile", "list"]).await;
    assert_eq!(code, 0, "{listed}");
    let profiles = listed["ok"]["profiles"].as_array().expect("profiles");
    let named = |name: &str| {
        profiles
            .iter()
            .find(|entry| entry["name"] == name)
            .unwrap_or_else(|| panic!("{name} is listed: {listed}"))
    };
    assert_eq!(named("wrong-id")["state"], "needs_migration", "{listed}");
    let entry = named("work");
    assert_eq!(entry["state"], "package_unavailable", "{listed}");
    let detail = entry["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("pohunek plugin doctor"), "{listed}");
    // The profile is already pinned to the digest, so migrating cannot help.
    assert!(!detail.contains("plugin profile migrate"), "{listed}");
    assert!(!detail.contains("plugin install"), "{listed}");

    harness.stop().await;
}

#[tokio::test]
async fn an_official_profile_refused_after_a_registry_rebuild_points_at_doctor() {
    let key = test_key();
    let anchor = HostTrustAnchor::new(vec![root_of(&key, WINDOW_START, WINDOW_END)], Vec::new())
        .expect("the test trust anchor");
    let harness = Harness::start_with_trust(CatalogTrust::Loaded(anchor)).await;
    harness.profile("codex-work", "base = \"codex\"\n");
    // Nothing serves or may serve `claude`: the damaged package is Codex's.
    harness.profile("claude-work", "base = \"claude\"\n");
    let official = harness.archive_of(OFFICIAL_CODEX_ID, "codex", "1.0.0");
    let (code, installed) =
        install_official(&harness, &key, &official, OFFICIAL_CODEX_ID, "codex").await;
    assert_eq!(code, 0, "{installed}");
    tamper(&harness, &official.digest);
    // Installing another package rebuilds the runtime registry, which leaves
    // the tampered official package out of it.
    let other = harness.archive("1.0.0");
    install_enabled(&harness, &other).await;

    let (code, listed) = harness.json(&["plugin", "profile", "list"]).await;
    assert_eq!(code, 0, "{listed}");
    let entry = listed["ok"]["profiles"]
        .as_array()
        .expect("profiles")
        .iter()
        .find(|entry| entry["name"] == "codex-work")
        .expect("codex-work is listed")
        .clone();
    assert_eq!(entry["state"], "package_unavailable", "{entry}");

    let (code, refused) = launch(&harness, "codex-work").await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "runtime_incompatible", "{refused}");
    let recover = refused["err"]["recover"].as_str().unwrap_or_default();
    assert!(recover.contains("pohunek plugin doctor"), "{refused}");
    assert!(!recover.contains("plugin install"), "{refused}");

    let claude = listed["ok"]["profiles"]
        .as_array()
        .expect("profiles")
        .iter()
        .find(|entry| entry["name"] == "claude-work")
        .expect("claude-work is listed")
        .clone();
    assert_eq!(claude["state"], "package_not_installed", "{claude}");
    let (code, refused) = launch(&harness, "claude-work").await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "runtime_not_installed", "{refused}");
    let recover = refused["err"]["recover"].as_str().unwrap_or_default();
    assert!(recover.contains("plugin install"), "{refused}");

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

#[tokio::test]
async fn session_list_names_the_code_resume_and_fork_refuse_a_damaged_package_session_with() {
    let harness = Harness::start().await;
    let built = harness.archive("1.0.0");
    install_enabled(&harness, &built).await;
    let (code, created) = launch(&harness, RUNTIME).await;
    assert_eq!(code, 0, "{created}");
    let id = created["ok"]["id"]
        .as_str()
        .expect("created session id")
        .to_owned();
    assert_eq!(
        created["ok"]["capabilities"],
        serde_json::json!({"resume": true, "fork": true}),
        "{created}"
    );
    assert!(
        created["ok"].get("capability_refusals").is_none(),
        "{created}"
    );
    tamper(&harness, &built.digest);
    // Installing another package rebuilds the runtime registry, which leaves
    // the tampered package out of it.
    let other = harness.archive_of("acme.runtime.other", "other", "1.0.0");
    install_enabled(&harness, &other).await;

    let (code, resumed) = harness.json(&["session", "resume", &id]).await;
    assert_eq!(code, 1, "{resumed}");
    let resume_code = resumed["err"]["code"]
        .as_str()
        .expect("resume refusal code");
    let (code, forked) = harness.json(&["session", "fork", &id]).await;
    assert_eq!(code, 1, "{forked}");
    let fork_code = forked["err"]["code"].as_str().expect("fork refusal code");

    let (code, listed) = harness.json(&["session", "list"]).await;
    assert_eq!(code, 0, "{listed}");
    let session = listed["ok"]
        .as_array()
        .expect("session list array")
        .iter()
        .find(|session| session["id"] == id.as_str())
        .expect("the session is listed")
        .clone();
    assert_eq!(
        session["capabilities"],
        serde_json::json!({"resume": false, "fork": false}),
        "{session}"
    );
    assert_eq!(
        session["capability_refusals"],
        serde_json::json!({"resume": resume_code, "fork": fork_code}),
        "{session}"
    );

    let human = harness.run(&["session", "list"]).await;
    assert_eq!(human.status.code(), Some(0), "{}", stderr_text(&human));
    let line = format!("refused {id}: resume={resume_code} fork={fork_code}");
    assert!(
        stdout_text(&human)
            .lines()
            .any(|candidate| candidate == line),
        "{line} in {}",
        stdout_text(&human)
    );

    harness.stop().await;
}
