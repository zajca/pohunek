//! The production-format Hermes package fixture (`compat/hermes/package`).
//!
//! The scenario builds the package archive, installs it through the CLI and
//! daemon using a throwaway signed catalog, and checks the selected runtime
//! through the host-inspection contract. It also verifies failed installs do
//! not replace that selection.

// Rust guideline compliant 2026-10-11

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt as _};
use std::path::{Path, PathBuf};

use package::{build_archive, read_archive, ArchiveEntry, CatalogEntry, Limits, PackageDigest};
use pohunek_daemon::catalog_anchor::CatalogTrust;
use pohunek_daemon::session::HostTrustAnchor;
use pohunek_test_support::bin_exe;
use pohunek_test_support::fs::write_executable;
use pohunek_test_support::workspace_root;
use protocol::{PackageId, PackageVersion, RuntimeId};
use serde_json::Value;

#[path = "support/catalog_fixture.rs"]
mod catalog_fixture;
#[path = "support/interpreter.rs"]
mod interpreter;
#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use catalog_fixture::{
    attestations_for, catalog_of, host_platform, root_of, signed, test_key, ANY_CORE, RUNTIME_API,
    WINDOW_END, WINDOW_START,
};
use plugin_harness::{path_str, Harness};

/// Directory of the package fixture, relative to the workspace root.
const PACKAGE_DIR: &str = "compat/hermes/package";

/// Compatibility lock of the package, relative to the workspace root.
const LOCK_PATH: &str = "compat/hermes/compatibility-lock.json";

/// Compiled Hermes detection manifest, used as the package parity source.
const BUILTIN_MANIFEST: &str = "crates/daemon/src/detect/manifests/hermes.toml";

/// Compiled Hermes descriptor, used as the package parity source.
const BUILTIN_DESCRIPTOR: &str = "crates/daemon/src/agent/builtin/hermes.toml";

/// Package id the descriptor declares.
const PACKAGE_ID: &str = "pohunek.runtime.hermes";

/// Version of the package the descriptor declares.
const PACKAGE_VERSION: &str = "1.0.0";

/// Profile that pins the runtime package in the installation scenario.
const PROFILE: &str = "hermes-package-test";

fn package_dir() -> PathBuf {
    workspace_root().join(PACKAGE_DIR)
}

fn read_toml(relative: &str) -> toml::Table {
    fs::read_to_string(workspace_root().join(relative))
        .expect("read the TOML file")
        .parse()
        .expect("the file is TOML")
}

/// Builds the package directory into its canonical archive and content digest.
fn built_archive() -> (Vec<u8>, PackageDigest) {
    let root = package_dir();
    let sidecar_root = workspace_root().join("compat/hermes");
    let mut entries = Vec::new();
    for (path, file) in [
        ("detect.toml", root.join("detect.toml")),
        ("runtime.toml", root.join("runtime.toml")),
        ("operator.toml", sidecar_root.join("operator.toml")),
    ] {
        entries.push(ArchiveEntry {
            path: path.to_owned(),
            contents: fs::read(file).expect("read package fixture file"),
            executable: false,
        });
    }
    for path in [
        "plugin.yaml",
        "__init__.py",
        "cli.py",
        "hooks.py",
        "policy.py",
        "redact.py",
        "tools.py",
        "skills/pohunek/SKILL.md",
    ] {
        entries.push(ArchiveEntry {
            path: format!("operator/{path}"),
            contents: fs::read(sidecar_root.join("operator").join(path))
                .expect("read operator payload"),
            executable: false,
        });
    }
    let bytes = build_archive(&entries, &Limits::DEFAULT).expect("the package archive builds");
    let digest = read_archive(&bytes, &Limits::DEFAULT)
        .expect("the archive reads")
        .digest()
        .clone();
    (bytes, digest)
}

/// Reports which package the daemon selected for the reserved `hermes` id.
async fn selected_package(harness: &Harness, digest: &PackageDigest) {
    let (code, listed) = harness.json(&["plugin", "list"]).await;
    assert_eq!(code, 0, "{listed}");
    let packages = listed["ok"]["packages"].as_array().expect("package list");
    assert_eq!(packages.len(), 1, "{listed}");
    let package = &packages[0];
    assert_eq!(package["package"]["id"], PACKAGE_ID, "{listed}");
    assert_eq!(package["origin"], "official", "{listed}");
    assert_eq!(package["enabled"], true, "{listed}");
    assert_eq!(package["selected"], true, "{listed}");
    assert_eq!(package["digest"], digest.as_str(), "{listed}");
}

/// Writes a profile pinned to the installed package and an isolated probe binary.
fn write_profile(harness: &Harness, digest: &PackageDigest, program: &Path) {
    harness.profile(
        PROFILE,
        &format!(
            "base = \"hermes\"\npackage = \"{PACKAGE_ID}\"\ndigest = \"{}\"\nprogram = \"{}\"\n",
            digest.as_str(),
            path_str(program),
        ),
    );
}

/// Makes a probe binary that prints one Hermes release banner.
fn write_probe(harness: &Harness, name: &str, banner: &str) -> PathBuf {
    let path = harness.env.root().join(name);
    write_executable(
        &path,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf '%s\\n' '{banner}'; exit 0; fi\nexit 97\n"
        ),
    )
    .expect("write the probe executable");
    path
}

/// Checks package version support through `pohunek host inspect`.
async fn inspect_profile_version(harness: &Harness, expected_version: &str, supported: bool) {
    let (code, host) = harness.json(&["host", "inspect", "local"]).await;
    assert_eq!(code, 0, "{host}");
    let runtime = host["ok"]["runtimes"]
        .as_array()
        .expect("runtime inventory")
        .iter()
        .find(|runtime| runtime["agent"] == PROFILE)
        .unwrap_or_else(|| panic!("host inventory contains {PROFILE}: {host}"));
    assert_eq!(runtime["available"], true, "{host}");
    assert_eq!(runtime["version"], expected_version, "{host}");
    assert_eq!(runtime["supported"], supported, "{host}");
}

/// Installs the built package through a throwaway signed catalog.
async fn install_official(
    harness: &Harness,
    key: &ed25519_dalek::SigningKey,
) -> (Vec<u8>, PackageDigest, PathBuf, PathBuf) {
    let (bytes, digest) = built_archive();
    let archive = harness.env.root().join("hermes.tar.zst");
    fs::write(&archive, &bytes).expect("write the package archive");

    let platform = host_platform();
    let entry = CatalogEntry {
        package_id: PackageId::parse(PACKAGE_ID).expect("package id"),
        runtime_id: RuntimeId::parse("hermes").expect("runtime id"),
        version: PackageVersion::parse(PACKAGE_VERSION).expect("package version"),
        digest: digest.clone(),
        runtime_api: RUNTIME_API,
        platforms: vec![platform.clone()],
        attestations: attestations_for(std::slice::from_ref(&platform)),
        core: ANY_CORE.to_owned(),
    };
    let catalog = harness.env.root().join("runtime-catalog.json");
    fs::write(
        &catalog,
        signed(key, catalog_of(1, WINDOW_END, vec![entry])),
    )
    .expect("write the signed catalog");

    let (code, installed) = harness
        .json(&[
            "plugin",
            "install",
            path_str(&archive),
            "--catalog",
            path_str(&catalog),
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{installed}");
    assert_eq!(installed["ok"]["status"], "installed", "{installed}");
    assert_eq!(
        installed["ok"]["package"]["origin"], "official",
        "{installed}"
    );
    assert_eq!(installed["ok"]["package"]["digest"], digest.as_str());

    let archive_entries = read_archive(&bytes, &Limits::DEFAULT).expect("the archive reads");
    let paths: Vec<&str> = archive_entries
        .entries()
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    assert_eq!(
        paths,
        [
            "detect.toml",
            "operator.toml",
            "operator/__init__.py",
            "operator/cli.py",
            "operator/hooks.py",
            "operator/plugin.yaml",
            "operator/policy.py",
            "operator/redact.py",
            "operator/skills/pohunek/SKILL.md",
            "operator/tools.py",
            "runtime.toml"
        ]
    );

    (bytes, digest, archive, catalog)
}

/// Checks the exact locked release and the first release outside the range.
async fn assert_supported_boundary(
    harness: &Harness,
    digest: &PackageDigest,
    locked_release: &str,
    locked_banner: &str,
) {
    let supported_probe = write_probe(harness, "hermes-supported", locked_banner);
    write_profile(harness, digest, &supported_probe);
    inspect_profile_version(harness, locked_release, true).await;

    let unsupported_probe = write_probe(harness, "hermes-next", "Hermes Agent v0.20.1 (future)");
    write_profile(harness, digest, &unsupported_probe);
    inspect_profile_version(harness, "0.20.1", false).await;
    write_profile(harness, digest, &supported_probe);
    inspect_profile_version(harness, locked_release, true).await;
}

/// Confirms unsigned reserved-id and corrupt archive inputs leave selection untouched.
async fn assert_failed_installs_preserve_selection(
    harness: &Harness,
    digest: &PackageDigest,
    bytes: &[u8],
    archive: &Path,
    catalog: &Path,
) {
    let (code, refused) = harness
        .json(&[
            "plugin",
            "install",
            path_str(archive),
            "--sha256",
            digest.as_str(),
            "--yes",
        ])
        .await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(
        refused["err"]["code"], "package_runtime_not_claimable",
        "a correct digest cannot authorize a reserved runtime id: {refused}"
    );
    selected_package(harness, digest).await;

    let corrupt_archive = harness.env.root().join("hermes-corrupt.tar.zst");
    let mut corrupt_bytes = bytes.to_vec();
    corrupt_bytes[0] ^= 0xff;
    fs::write(&corrupt_archive, corrupt_bytes).expect("write the corrupt archive");
    let (code, refused) = harness
        .json(&[
            "plugin",
            "install",
            path_str(&corrupt_archive),
            "--catalog",
            path_str(catalog),
            "--yes",
        ])
        .await;
    assert_eq!(code, 1, "{refused}");
    assert!(refused["err"]["code"].is_string(), "{refused}");
    selected_package(harness, digest).await;
}

/// Verifies the installed official Hermes package and its failure boundaries.
#[tokio::test]
async fn the_signed_hermes_package_is_selected_and_failed_installs_preserve_it() {
    let strip_package_specific_fields = |mut table: toml::Table| {
        let runtime = table
            .get_mut("runtime")
            .and_then(toml::Value::as_table_mut)
            .expect("a [runtime] table");
        assert!(runtime.remove("detect_manifest").is_some());
        assert!(runtime.remove("version_probe").is_some());
        table
    };
    assert_eq!(
        strip_package_specific_fields(read_toml(&format!("{PACKAGE_DIR}/runtime.toml"))),
        strip_package_specific_fields(read_toml(BUILTIN_DESCRIPTOR))
    );

    let package_manifest = fs::read(package_dir().join("detect.toml")).expect("package manifest");
    let builtin_manifest =
        fs::read(workspace_root().join(BUILTIN_MANIFEST)).expect("built-in manifest");
    assert_eq!(package_manifest, builtin_manifest);

    let lock: Value = serde_json::from_slice(
        &fs::read(workspace_root().join(LOCK_PATH)).expect("read compatibility lock"),
    )
    .expect("the compatibility lock is JSON");
    let locked_release = lock["release"].as_str().expect("locked release");
    assert_eq!(locked_release, "0.20.0");
    let locked_banner = lock["cli_checks"]
        .as_array()
        .expect("CLI checks")
        .iter()
        .find(|check| check["id"] == "version")
        .and_then(|check| check["required_text"][0].as_str())
        .expect("the lock has a required version banner");

    let key = test_key();
    let trust = HostTrustAnchor::new(vec![root_of(&key, WINDOW_START, WINDOW_END)], Vec::new())
        .expect("the test trust anchor");
    let harness = Harness::start_with_trust(CatalogTrust::Loaded(trust)).await;
    let (bytes, digest, archive, catalog) = install_official(&harness, &key).await;
    assert_operator_lifecycle(
        &harness,
        &digest,
        &key,
        &bytes,
        &archive,
        &catalog,
        locked_release,
        locked_banner,
    )
    .await;
    harness.stop().await;
}

#[expect(
    clippy::too_many_lines,
    reason = "one package-boundary scenario checks source selection and fail-closed actions"
)]
async fn assert_no_selected_official_package(
    harness: &Harness,
    digest: &PackageDigest,
    key: &ed25519_dalek::SigningKey,
    hermes_bin: &Path,
    install_args: &[&str],
) {
    let (original, _) = built_archive();
    let parsed = read_archive(&original, &Limits::DEFAULT).expect("original archive");
    let mut entries = parsed
        .entries()
        .iter()
        .map(|entry| ArchiveEntry {
            path: entry.path.clone(),
            contents: entry.contents.clone(),
            executable: entry.executable,
        })
        .collect::<Vec<_>>();
    let runtime = entries
        .iter_mut()
        .find(|entry| entry.path == "runtime.toml")
        .expect("runtime descriptor");
    let descriptor = String::from_utf8(runtime.contents.clone()).expect("descriptor UTF-8");
    runtime.contents = descriptor
        .replace("version = \"1.0.0\"", "version = \"1.0.1\"")
        .into_bytes();
    let bytes = build_archive(&entries, &Limits::DEFAULT).expect("alternate archive");
    let versioned = read_archive(&bytes, &Limits::DEFAULT).expect("alternate archive reads");
    let alternate_digest = versioned.digest().clone();
    let archive = harness.env.root().join("hermes-1.0.1.tar.zst");
    fs::write(&archive, bytes).expect("write alternate archive");
    let platform = host_platform();
    let entry = CatalogEntry {
        package_id: PackageId::parse(PACKAGE_ID).expect("package id"),
        runtime_id: RuntimeId::parse("hermes").expect("runtime id"),
        version: PackageVersion::parse("1.0.1").expect("alternate package version"),
        digest: alternate_digest.clone(),
        runtime_api: RUNTIME_API,
        platforms: vec![platform.clone()],
        attestations: attestations_for(std::slice::from_ref(&platform)),
        core: ANY_CORE.to_owned(),
    };
    let catalog = harness.env.root().join("runtime-catalog-2.json");
    fs::write(
        &catalog,
        signed(key, catalog_of(2, WINDOW_END, vec![entry])),
    )
    .expect("write updated signed catalog");
    let updated = harness
        .json(&[
            "plugin",
            "update",
            PACKAGE_ID,
            path_str(&archive),
            "--catalog",
            path_str(&catalog),
            "--yes",
        ])
        .await;
    assert_eq!(
        updated.0, 0,
        "install alternate Hermes version: {:#}",
        updated.1
    );
    assert_eq!(
        updated.1["ok"]["package"]["digest"],
        alternate_digest.as_str()
    );
    let removed = harness
        .json(&[
            "plugin",
            "uninstall",
            PACKAGE_ID,
            "--digest",
            alternate_digest.as_str(),
            "--yes",
        ])
        .await;
    assert_eq!(
        removed.0, 0,
        "remove selected alternate version: {:#}",
        removed.1
    );

    let (listed_code, listed) = harness.json(&["plugin", "list"]).await;
    assert_eq!(listed_code, 0, "list unselected Hermes package: {listed:#}");
    let package = listed["ok"]["packages"]
        .as_array()
        .expect("package list")
        .iter()
        .find(|package| package["package"]["id"] == PACKAGE_ID)
        .expect("original package remains installed");
    assert_eq!(package["selected"], false);

    let status_args = [
        "integration",
        "status",
        "--agent",
        "hermes",
        "--hermes-profile",
        "default",
        "--hermes-bin",
        path_str(hermes_bin),
    ];
    let status = harness.json(&status_args).await;
    assert_eq!(
        status.0, 0,
        "status reports unselected source: {:#}",
        status.1
    );
    assert_eq!(status.1["ok"]["package_source_missing"], true);
    let doctor = harness
        .json(&[
            "integration",
            "doctor",
            "--agent",
            "hermes",
            "--hermes-profile",
            "default",
            "--hermes-bin",
            path_str(hermes_bin),
        ])
        .await;
    assert!(doctor.1["ok"]["doctor"]["checks"]
        .as_array()
        .expect("doctor checks")
        .iter()
        .any(|check| check["code"] == "asset_source" && check["status"] == "fail"));
    let rejected = harness.json(install_args).await;
    assert_eq!(
        rejected.0, 1,
        "install rejects unselected source: {:#}",
        rejected.1
    );
    let rejected = harness
        .json(&[
            "integration",
            "update",
            "--agent",
            "hermes",
            "--hermes-profile",
            "default",
            "--hermes-bin",
            path_str(hermes_bin),
        ])
        .await;
    assert_eq!(
        rejected.0, 1,
        "update rejects unselected source: {:#}",
        rejected.1
    );
    let selected = harness
        .json(&["plugin", "select", PACKAGE_ID, "--digest", digest.as_str()])
        .await;
    assert_eq!(
        selected.0, 0,
        "restore Hermes package selection: {:#}",
        selected.1
    );
}

#[expect(
    clippy::too_many_lines,
    reason = "one supported-boundary scenario exercises the complete operator lifecycle"
)]
#[expect(
    clippy::too_many_arguments,
    reason = "the lifecycle scenario receives the exact signed fixture inputs"
)]
async fn assert_operator_lifecycle(
    harness: &Harness,
    digest: &PackageDigest,
    key: &ed25519_dalek::SigningKey,
    package_bytes: &[u8],
    archive: &Path,
    catalog: &Path,
    locked_release: &str,
    locked_banner: &str,
) {
    let hermes_home = harness.env.home().join(".hermes");
    let install_bin = harness.env.root().join("hermes-install/venv/bin");
    let python_bin = harness.env.root().join("hermes-install/python/bin");
    for directory in [
        hermes_home.join("profiles/default"),
        hermes_home.join("profiles"),
        hermes_home.clone(),
        install_bin.clone(),
        install_bin.parent().expect("venv parent").to_path_buf(),
        install_bin
            .parent()
            .expect("venv parent")
            .parent()
            .expect("install parent")
            .join("python"),
        python_bin.clone(),
    ] {
        fs::create_dir_all(&directory).expect("create Hermes fixture directory");
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
            .expect("set Hermes fixture directory mode");
    }
    write_executable(
        python_bin.join("python3"),
        format!("exec '{}' \"$@\"", interpreter::python().display()),
    )
    .expect("write controlled Hermes Python");
    symlink("../../python/bin/python3", install_bin.join("python3"))
        .expect("link contained Hermes Python runtime");

    let plugin_root = hermes_home.join("plugins/operators/pohunek");
    let state_file = harness.env.root().join("hermes-plugin-state");
    let enable_failure = harness.env.root().join("fail-hermes-enable");
    fs::write(&state_file, "disabled\n").expect("initialize Hermes plugin state");
    let hermes_bin = install_bin.join("hermes");
    write_executable(
        &hermes_bin,
        format!(
            "case \"$*\" in\n--version) printf '%s\\n' 'Hermes Agent v0.20.0 (2026.8.3)' ;;\n'plugins list --json') if [ -d '{}' ]; then printf '[{{\"name\":\"pohunek\",\"status\":\"%s\",\"source\":\"user\"}}]\\n' \"$(/bin/cat '{}')\"; else printf '%s\\n' '[]'; fi ;;\n'plugins enable pohunek --no-allow-tool-override') if [ -e '{}' ]; then exit 90; fi; printf '%s\\n' enabled > '{}' ;;\n'plugins disable pohunek') printf '%s\\n' disabled > '{}' ;;\n*) exit 90 ;;\nesac",
            plugin_root.display(),
            state_file.display(),
            enable_failure.display(),
            state_file.display(),
            state_file.display(),
        ),
    )
    .expect("write controlled Hermes executable");

    let pohunek_bin = bin_exe("pohunek");
    let install_args = [
        "integration",
        "install",
        "--agent",
        "hermes",
        "--hermes-profile",
        "default",
        "--hermes-bin",
        path_str(&hermes_bin),
        "--pohunek-bin",
        path_str(&pohunek_bin),
        "--access-mode",
        "manage",
        "--allow-host",
        "local",
    ];
    let status_args = [
        "integration",
        "status",
        "--agent",
        "hermes",
        "--hermes-profile",
        "default",
        "--hermes-bin",
        path_str(&hermes_bin),
    ];

    assert_no_selected_official_package(harness, digest, key, &hermes_bin, &install_args).await;
    selected_package(harness, digest).await;
    assert_supported_boundary(harness, digest, locked_release, locked_banner).await;
    assert_failed_installs_preserve_selection(harness, digest, package_bytes, archive, catalog)
        .await;
    let selected_status = harness.json(&status_args).await;
    assert_eq!(
        selected_status.0, 0,
        "status after source selection: {:#}",
        selected_status.1
    );
    assert_eq!(selected_status.1["ok"]["package_source_missing"], false);

    let colliding_plugin = hermes_home.join("plugins/operators/pohunek");
    fs::create_dir_all(&colliding_plugin).expect("create colliding plugin directory");
    fs::set_permissions(
        colliding_plugin.parent().expect("operator parent"),
        fs::Permissions::from_mode(0o700),
    )
    .expect("make operator parent private");
    fs::set_permissions(&colliding_plugin, fs::Permissions::from_mode(0o700))
        .expect("make colliding plugin private");
    fs::write(colliding_plugin.join("owner-file"), b"preserve")
        .expect("write unrelated plugin data");
    fs::set_permissions(
        colliding_plugin.join("owner-file"),
        fs::Permissions::from_mode(0o600),
    )
    .expect("make collision file private");
    let collision = harness.json(&install_args).await;
    assert_eq!(
        collision.0, 1,
        "unowned plugin collision is refused: {:#}",
        collision.1
    );
    assert_eq!(
        fs::read(colliding_plugin.join("owner-file")).expect("preserved collision"),
        b"preserve"
    );
    fs::remove_dir_all(colliding_plugin).expect("remove fixture collision");

    let install = harness.json(&install_args).await;
    assert_eq!(install.0, 0, "package-backed install: {:#}", install.1);
    let marker_path = plugin_root.join(".pohunek-owned.json");
    let marker: Value = serde_json::from_slice(&fs::read(&marker_path).expect("read marker"))
        .expect("parse marker");
    assert_eq!(marker["source"], "package");
    assert_eq!(marker["package_id"], PACKAGE_ID);
    assert_eq!(
        marker["package_digest"],
        digest.as_str().trim_start_matches("sha256:")
    );
    let init = fs::read_to_string(plugin_root.join("__init__.py")).expect("read rendered init");
    let policy_path = marker["policy_path"].as_str().expect("marker policy path");
    assert!(!init.contains("__POHUNEK_POLICY_PATH__"));
    assert!(
        init.contains(policy_path),
        "the policy token binds the exact policy path"
    );

    let disabled = harness
        .json(&["plugin", "disable", PACKAGE_ID, "--digest", digest.as_str()])
        .await;
    assert_eq!(disabled.0, 0, "disable Hermes package: {:#}", disabled.1);
    let status = harness.json(&status_args).await;
    assert_eq!(
        status.0, 0,
        "status reports disabled source: {:#}",
        status.1
    );
    assert_eq!(status.1["ok"]["package_source_missing"], true);
    let doctor = harness
        .json(&[
            "integration",
            "doctor",
            "--agent",
            "hermes",
            "--hermes-profile",
            "default",
            "--hermes-bin",
            path_str(&hermes_bin),
        ])
        .await;
    assert_eq!(doctor.1["ok"]["doctor"]["ok"], false);
    assert!(doctor.1["ok"]["doctor"]["checks"]
        .as_array()
        .expect("doctor checks")
        .iter()
        .any(|check| check["code"] == "asset_source" && check["status"] == "fail"));
    let rejected = harness.json(&install_args).await;
    assert_eq!(
        rejected.0, 1,
        "install rejects disabled source: {:#}",
        rejected.1
    );
    let rejected = harness
        .json(&[
            "integration",
            "update",
            "--agent",
            "hermes",
            "--hermes-profile",
            "default",
            "--hermes-bin",
            path_str(&hermes_bin),
        ])
        .await;
    assert_eq!(
        rejected.0, 1,
        "update rejects disabled source: {:#}",
        rejected.1
    );
    let enabled = harness
        .json(&["plugin", "enable", PACKAGE_ID, "--digest", digest.as_str()])
        .await;
    assert_eq!(enabled.0, 0, "restore Hermes package: {:#}", enabled.1);

    let status = harness.json(&status_args).await;
    assert_eq!(status.0, 0, "status: {:#}", status.1);
    assert_eq!(status.1["ok"]["package_source_missing"], false);

    let operator_root = harness.root_of(digest).join("files/operator");
    let tools = operator_root.join("tools.py");
    let tools_bytes = fs::read(&tools).expect("snapshot package file");
    fs::write(&tools, b"tampered").expect("tamper package file");
    let rejected = harness.json(&status_args).await;
    assert_eq!(
        rejected.0, 0,
        "status reports corrupt source: {:#}",
        rejected.1
    );
    assert_eq!(rejected.1["ok"]["package_source_missing"], true);
    let doctor = harness
        .json(&[
            "integration",
            "doctor",
            "--agent",
            "hermes",
            "--hermes-profile",
            "default",
            "--hermes-bin",
            path_str(&hermes_bin),
        ])
        .await;
    assert_eq!(doctor.1["ok"]["doctor"]["ok"], false);
    assert!(doctor.1["ok"]["doctor"]["checks"]
        .as_array()
        .expect("doctor checks")
        .iter()
        .any(|check| check["code"] == "asset_source" && check["status"] == "fail"));
    let rejected = harness.json(&install_args).await;
    assert_eq!(
        rejected.0, 1,
        "install rejects corrupt source: {:#}",
        rejected.1
    );
    let rejected = harness
        .json(&[
            "integration",
            "update",
            "--agent",
            "hermes",
            "--hermes-profile",
            "default",
            "--hermes-bin",
            path_str(&hermes_bin),
        ])
        .await;
    assert_eq!(
        rejected.0, 1,
        "update rejects corrupt source: {:#}",
        rejected.1
    );
    fs::write(&tools, tools_bytes).expect("restore package file");
    fs::set_permissions(&tools, fs::Permissions::from_mode(0o600)).expect("restore package mode");

    let extra = operator_root.join("extra.py");
    fs::write(&extra, b"extra").expect("write unmanifested operator asset");
    fs::set_permissions(&extra, fs::Permissions::from_mode(0o600)).expect("set extra asset mode");
    let rejected = harness.json(&status_args).await;
    assert_eq!(
        rejected.0, 0,
        "status reports extra source data: {:#}",
        rejected.1
    );
    assert_eq!(rejected.1["ok"]["package_source_missing"], true);
    fs::remove_file(extra).expect("remove extra asset");

    let missing = operator_root.join("redact.py");
    let missing_bytes = fs::read(&missing).expect("snapshot required package file");
    fs::remove_file(&missing).expect("remove required package file");
    let rejected = harness.json(&status_args).await;
    assert_eq!(
        rejected.0, 0,
        "status reports missing source data: {:#}",
        rejected.1
    );
    assert_eq!(rejected.1["ok"]["package_source_missing"], true);
    fs::write(&missing, missing_bytes).expect("restore required package file");
    fs::set_permissions(&missing, fs::Permissions::from_mode(0o600)).expect("restore package mode");

    let repeated = harness.json(&install_args).await;
    assert_eq!(
        repeated.0, 0,
        "repeat install is idempotent: {:#}",
        repeated.1
    );

    let marker_before = fs::read(&marker_path).expect("snapshot marker before update");
    fs::write(&enable_failure, b"fail").expect("arm Hermes enable failure");
    let rejected = harness
        .json(&[
            "integration",
            "update",
            "--agent",
            "hermes",
            "--hermes-profile",
            "default",
            "--hermes-bin",
            path_str(&hermes_bin),
            "--pohunek-bin",
            path_str(&pohunek_bin),
            "--access-mode",
            "full",
        ])
        .await;
    assert_eq!(
        rejected.0, 1,
        "failed Hermes enable rolls update back: {:#}",
        rejected.1
    );
    fs::remove_file(enable_failure).expect("disarm Hermes enable failure");
    assert_eq!(
        fs::read(&marker_path).expect("marker after rollback"),
        marker_before
    );

    fs::remove_file(harness.agents.join(format!("{PROFILE}.toml")))
        .expect("remove the profile pin before source removal");
    let package_remove = harness
        .json(&[
            "plugin",
            "uninstall",
            PACKAGE_ID,
            "--digest",
            digest.as_str(),
            "--yes",
        ])
        .await;
    assert_eq!(
        package_remove.0, 0,
        "remove package source: {:#}",
        package_remove.1
    );
    let status = harness.json(&status_args).await;
    assert_eq!(status.0, 0, "status reports missing source: {:#}", status.1);
    assert_eq!(status.1["ok"]["package_source_missing"], true);
    let doctor = harness
        .json(&[
            "integration",
            "doctor",
            "--agent",
            "hermes",
            "--hermes-profile",
            "default",
            "--hermes-bin",
            path_str(&hermes_bin),
        ])
        .await;
    assert_eq!(doctor.1["ok"]["doctor"]["ok"], false);
    assert!(doctor.1["ok"]["doctor"]["checks"]
        .as_array()
        .expect("doctor checks")
        .iter()
        .any(|check| check["code"] == "asset_source" && check["status"] == "fail"));
    let rejected = harness
        .json(&[
            "integration",
            "update",
            "--agent",
            "hermes",
            "--hermes-profile",
            "default",
            "--hermes-bin",
            path_str(&hermes_bin),
            "--access-mode",
            "full",
        ])
        .await;
    assert_eq!(
        rejected.0, 1,
        "update cannot switch to embedded source: {:#}",
        rejected.1
    );
    let uninstall = harness
        .json(&[
            "integration",
            "uninstall",
            "--agent",
            "hermes",
            "--hermes-profile",
            "default",
            "--hermes-bin",
            path_str(&hermes_bin),
        ])
        .await;
    assert_eq!(
        uninstall.0, 0,
        "marker-backed uninstall survives missing source: {:#}",
        uninstall.1
    );
    assert!(!plugin_root.exists());
}
