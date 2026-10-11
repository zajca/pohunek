//! The production-format Hermes package fixture (`compat/hermes/package`).
//!
//! The scenario builds the package archive, installs it through the CLI and
//! daemon using a throwaway signed catalog, and checks the selected runtime
//! through the host-inspection contract. It also verifies failed installs do
//! not replace that selection.

// Rust guideline compliant 2026-10-11

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};

use package::directory::build_directory_archive;
use package::{read_archive, CatalogEntry, Limits, PackageDigest};
use pohunek_daemon::catalog_anchor::CatalogTrust;
use pohunek_daemon::session::HostTrustAnchor;
use pohunek_test_support::fs::write_executable;
use pohunek_test_support::workspace_root;
use protocol::{PackageId, PackageVersion, RuntimeId};
use serde_json::Value;

#[path = "support/catalog_fixture.rs"]
mod catalog_fixture;
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
    let bytes = build_directory_archive(&package_dir(), &Limits::DEFAULT)
        .expect("the package directory builds");
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
    assert_eq!(paths, ["detect.toml", "runtime.toml"]);

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
    selected_package(&harness, &digest).await;
    assert_supported_boundary(&harness, &digest, locked_release, locked_banner).await;
    assert_failed_installs_preserve_selection(&harness, &digest, &bytes, &archive, &catalog).await;
    harness.stop().await;
}
