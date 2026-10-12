//! The production-format Hermes package fixture (`compat/hermes/package`) and
//! the Hermes operator lifecycle that takes its assets from that package.
//!
//! Every scenario builds package archives, installs them through the CLI and
//! daemon using a throwaway signed catalog, and drives `pohunek integration`
//! against a controlled Hermes executable. The packaged operator skill carries
//! one extra line, so installed bytes prove which source supplied them.
//!
//! One opt-in scenario drives a real pinned Hermes through the selected package
//! and the public session commands against a loopback model provider.

// Rust guideline compliant 2026-10-12

#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{symlink, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use package::{build_archive, read_archive, ArchiveEntry, CatalogEntry, Limits, PackageDigest};
use pohunek_daemon::agent::host::{definition_from_archive, HandlerId, ProbeVersion};
use pohunek_daemon::catalog_anchor::CatalogTrust;
use pohunek_daemon::session::HostTrustAnchor;
use pohunek_test_support::bin_exe;
use pohunek_test_support::fs::write_executable;
use pohunek_test_support::process_env::ProcessEnv;
use pohunek_test_support::wait::{guard, wait_until};
use pohunek_test_support::workspace_root;
use protocol::{PackageId, PackageVersion, RuntimeId};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[path = "support/catalog_fixture.rs"]
mod catalog_fixture;
#[path = "support/hermes_provider.rs"]
mod hermes_provider;
#[path = "support/interpreter.rs"]
mod interpreter;
#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use catalog_fixture::{
    attestations_for, catalog_of, host_platform, root_of, signed, test_key, ANY_CORE, RUNTIME_API,
    WINDOW_END, WINDOW_START,
};
use hermes_provider::{HermesProvider, API_KEY, MODEL};
use plugin_harness::{path_str, Harness};

/// Directory of the package fixture, relative to the workspace root.
const PACKAGE_DIR: &str = "compat/hermes/package";

/// Directory of the closed operator sidecar, relative to the workspace root.
const SIDECAR_DIR: &str = "compat/hermes";

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

/// The exact allowlisted operator payload of the closed sidecar.
const OPERATOR_PAYLOAD: [&str; 8] = [
    "plugin.yaml",
    "__init__.py",
    "cli.py",
    "hooks.py",
    "policy.py",
    "redact.py",
    "tools.py",
    "skills/pohunek/SKILL.md",
];

/// Operator skill path whose packaged bytes differ from the CLI's embedded copy.
const SKILL_PATH: &str = "skills/pohunek/SKILL.md";

/// Line appended to the packaged skill so installed bytes identify their source.
const PACKAGE_SKILL_LINE: &str = "\n<!-- packaged by the hermes_package fixture -->\n";

/// Placeholder the operator `__init__.py` must contain exactly once.
const POLICY_TOKEN: &str = "__POHUNEK_POLICY_PATH__";

/// Explicit opt-in for the real pinned Hermes package scenario.
const REAL_HERMES_VARIABLE: &str = "POHUNEK_HERMES_E2E";

/// Absolute executable path produced by `scripts/provision-hermes-compat`.
const HERMES_BINARY_VARIABLE: &str = "POHUNEK_HERMES_BIN";

/// The exact safe prompt submitted through the public session input command.
///
/// It does not contain [`REAL_TURN_RESPONSE`], so the response marker on the
/// screen can only come from the model reply, never from the echoed prompt.
const REAL_TURN_PROMPT: &str = "Reply with the Hermes package turn marker.";

/// Deterministic response emitted by the loopback Hermes provider.
const REAL_TURN_RESPONSE: &str = "HERMES_PACKAGE_TURN_OK";

/// Proxy variables that route every non-loopback request of a real Hermes
/// process to the fail-closed loopback provider.
const PROXY_VARIABLES: [&str; 6] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
];

/// Hosts a real Hermes process reaches directly instead of through the proxy.
const LOOPBACK_NO_PROXY: &str = "localhost,127.0.0.1";

/// Seconds the paused Hermes startup and its provider-named child may take to
/// meet the test-owned barrier. It bounds a hung real Hermes start instead of
/// letting the scenario wait forever; a cold Python import fits well within it.
const STARTUP_BARRIER_SECONDS: u64 = 20;

fn package_dir() -> PathBuf {
    workspace_root().join(PACKAGE_DIR)
}

fn sidecar_dir() -> PathBuf {
    workspace_root().join(SIDECAR_DIR)
}

fn read_toml(relative: &str) -> toml::Table {
    fs::read_to_string(workspace_root().join(relative))
        .expect("read the TOML file")
        .parse()
        .expect("the file is TOML")
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The operator payload file the CLI embeds, read from its source fixture.
fn embedded_operator_file(path: &str) -> Vec<u8> {
    fs::read(sidecar_dir().join("operator").join(path)).expect("read embedded operator payload")
}

fn entry_mut<'a>(entries: &'a mut [ArchiveEntry], path: &str) -> &'a mut ArchiveEntry {
    entries
        .iter_mut()
        .find(|entry| entry.path == path)
        .unwrap_or_else(|| panic!("archive entry {path}"))
}

fn entry<'a>(entries: &'a [ArchiveEntry], path: &str) -> &'a [u8] {
    &entries
        .iter()
        .find(|entry| entry.path == path)
        .unwrap_or_else(|| panic!("archive entry {path}"))
        .contents
}

/// Replaces one operator payload file and its recorded digest in `operator.toml`.
fn replace_operator_file(entries: &mut [ArchiveEntry], path: &str, contents: Vec<u8>) {
    let payload = entry_mut(entries, &format!("operator/{path}"));
    let previous = sha256_hex(&payload.contents);
    let current = sha256_hex(&contents);
    payload.contents = contents;
    let sidecar = entry_mut(entries, "operator.toml");
    let text = String::from_utf8(sidecar.contents.clone()).expect("sidecar UTF-8");
    assert_eq!(text.matches(&previous).count(), 1, "one digest for {path}");
    sidecar.contents = text.replacen(&previous, &current, 1).into_bytes();
}

/// Sets the package version in the runtime descriptor.
fn set_version(entries: &mut [ArchiveEntry], version: &str) {
    let runtime = entry_mut(entries, "runtime.toml");
    let descriptor = String::from_utf8(runtime.contents.clone()).expect("descriptor UTF-8");
    let declared = format!("version = \"{PACKAGE_VERSION}\"");
    assert_eq!(descriptor.matches(&declared).count(), 1);
    runtime.contents = descriptor
        .replacen(&declared, &format!("version = \"{version}\""), 1)
        .into_bytes();
}

/// The package fixture as archive entries, with a distinguishable operator skill.
fn package_entries() -> Vec<ArchiveEntry> {
    let root = package_dir();
    let mut entries = Vec::new();
    for (path, file) in [
        ("detect.toml", root.join("detect.toml")),
        ("runtime.toml", root.join("runtime.toml")),
        ("operator.toml", sidecar_dir().join("operator.toml")),
    ] {
        entries.push(ArchiveEntry {
            path: path.to_owned(),
            contents: fs::read(file).expect("read package fixture file"),
            executable: false,
        });
    }
    for path in OPERATOR_PAYLOAD {
        entries.push(ArchiveEntry {
            path: format!("operator/{path}"),
            contents: embedded_operator_file(path),
            executable: false,
        });
    }
    let mut skill = embedded_operator_file(SKILL_PATH);
    skill.extend_from_slice(PACKAGE_SKILL_LINE.as_bytes());
    replace_operator_file(&mut entries, SKILL_PATH, skill);
    entries
}

/// Builds archive entries into their canonical archive and content digest.
fn archive_of(entries: &[ArchiveEntry]) -> (Vec<u8>, PackageDigest) {
    let bytes = build_archive(entries, &Limits::DEFAULT).expect("the package archive builds");
    let digest = read_archive(&bytes, &Limits::DEFAULT)
        .expect("the archive reads")
        .digest()
        .clone();
    (bytes, digest)
}

/// Builds the package directory into its canonical archive and content digest.
fn built_archive() -> (Vec<u8>, PackageDigest) {
    archive_of(&package_entries())
}

fn catalog_entry(version: &str, digest: &PackageDigest) -> CatalogEntry {
    let platform = host_platform();
    CatalogEntry {
        package_id: PackageId::parse(PACKAGE_ID).expect("package id"),
        runtime_id: RuntimeId::parse("hermes").expect("runtime id"),
        version: PackageVersion::parse(version).expect("package version"),
        digest: digest.clone(),
        runtime_api: RUNTIME_API,
        platforms: vec![platform.clone()],
        attestations: attestations_for(std::slice::from_ref(&platform)),
        core: ANY_CORE.to_owned(),
    }
}

/// Starts a daemon whose host trusts the throwaway catalog signing key.
async fn trusted_harness(key: &ed25519_dalek::SigningKey) -> Harness {
    let trust = HostTrustAnchor::new(vec![root_of(key, WINDOW_START, WINDOW_END)], Vec::new())
        .expect("the test trust anchor");
    Harness::start_with_trust(CatalogTrust::Loaded(trust)).await
}

/// Writes `entries` as signed catalog `sequence` and returns the catalog path.
fn write_catalog(
    harness: &Harness,
    key: &ed25519_dalek::SigningKey,
    sequence: u64,
    entries: Vec<CatalogEntry>,
) -> PathBuf {
    let catalog = harness
        .env
        .root()
        .join(format!("runtime-catalog-{sequence}.json"));
    fs::write(
        &catalog,
        signed(key, catalog_of(sequence, WINDOW_END, entries)),
    )
    .expect("write the signed catalog");
    catalog
}

/// Writes one package archive below the harness root.
fn write_archive(harness: &Harness, name: &str, bytes: &[u8]) -> PathBuf {
    let archive = harness.env.root().join(name);
    fs::write(&archive, bytes).expect("write the package archive");
    archive
}

/// Installs a newer Hermes version, which the registry enables and selects.
async fn update_package(harness: &Harness, archive: &Path, catalog: &Path, digest: &PackageDigest) {
    let (code, updated) = harness
        .json(&[
            "plugin",
            "update",
            PACKAGE_ID,
            path_str(archive),
            "--catalog",
            path_str(catalog),
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "install Hermes package version: {updated:#}");
    assert_eq!(updated["ok"]["package"]["digest"], digest.as_str());
}

/// Selects one installed Hermes package version.
async fn select_package(harness: &Harness, digest: &PackageDigest) {
    let (code, selected) = harness
        .json(&["plugin", "select", PACKAGE_ID, "--digest", digest.as_str()])
        .await;
    assert_eq!(code, 0, "select Hermes package: {selected:#}");
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

/// Resolves the explicitly provisioned, private real Hermes executable.
fn require_real_hermes() -> PathBuf {
    assert_eq!(
        std::env::var(REAL_HERMES_VARIABLE).as_deref(),
        Ok("1"),
        "the real Hermes scenario needs {REAL_HERMES_VARIABLE}=1"
    );
    let path =
        PathBuf::from(std::env::var_os(HERMES_BINARY_VARIABLE).unwrap_or_else(|| {
            panic!("{HERMES_BINARY_VARIABLE} must name the provisioned Hermes")
        }));
    assert!(path.is_absolute(), "Hermes executable path is absolute");
    assert_eq!(
        fs::canonicalize(&path).expect("canonicalize pinned Hermes"),
        path,
        "Hermes executable path has no symlink components"
    );
    let metadata = fs::metadata(&path).expect("stat pinned Hermes");
    assert!(metadata.is_file(), "Hermes is a regular executable");
    assert!(
        metadata.permissions().mode() & 0o111 != 0,
        "Hermes is executable"
    );
    assert_eq!(
        metadata.permissions().mode() & 0o022,
        0,
        "Hermes is private"
    );
    assert!(
        metadata.uid() == 0 || metadata.uid() == nix::unistd::Uid::effective().as_raw(),
        "Hermes is owned by root or the test user"
    );
    path
}

/// The session worker journal for `session_id`, found below the private test root.
fn worker_journal(root: &Path, session_id: &str) -> Value {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(directory).expect("read private E2E state directory");
        for entry in entries {
            let entry = entry.expect("read private E2E state entry");
            let file_type = entry.file_type().expect("inspect private E2E state entry");
            if file_type.is_dir() {
                pending.push(entry.path());
                continue;
            }
            if !file_type.is_file() || entry.path().extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let Ok(bytes) = fs::read(entry.path()) else {
                continue;
            };
            let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
                continue;
            };
            if value["schema_version"].is_number() && value["session_id"] == session_id {
                return value;
            }
        }
    }
    panic!("the real worker journal exists for the paused E2E session");
}

/// Writes one private model-free Hermes configuration and its offline caches.
fn configure_real_hermes(home: &Path, provider: &HermesProvider) {
    fs::create_dir_all(home).expect("create isolated Hermes home");
    fs::set_permissions(home, fs::Permissions::from_mode(0o700)).expect("private Hermes home");
    let config = format!(
        "model:\n  default: {MODEL}\n  provider: custom:pohunek-compat\n  api_mode: chat_completions\n  context_length: 64000\nmodel_catalog:\n  enabled: false\nproviders:\n  pohunek-compat:\n    name: pohunek-compat\n    api: {}\n    api_key: {}\n    transport: chat_completions\n    default_model: {MODEL}\n    discover_models: false\n    models:\n      {MODEL}:\n        context_length: 64000\nfallback_providers: []\ntoolsets:\n  - terminal\napprovals:\n  mode: manual\n  timeout: 300\nsecurity:\n  tirith_enabled: false\n  allow_lazy_installs: false\nauxiliary:\n  title_generation:\n    enabled: false\ntelemetry:\n  shared_metrics:\n    enabled: false\n",
        provider.api_url(),
        API_KEY,
    );
    write_private(home.join("config.yaml"), config.as_bytes());
    write_private(
        home.join("models_dev_cache.json"),
        br#"{"pohunek-offline":{"name":"Pohunek offline compatibility cache","env":[],"api":"","doc":"","models":{}}}
"#,
    );
    fs::create_dir_all(home.join("cache")).expect("create model catalog cache");
    fs::set_permissions(home.join("cache"), fs::Permissions::from_mode(0o700))
        .expect("private model catalog cache");
    write_private(
        home.join("cache/model_catalog.json"),
        br#"{"version":1,"providers":{}}
"#,
    );
    write_private(
        home.join("auth.json"),
        br#"{"version":1,"providers":{},"suppressed_sources":{"copilot":["gh_cli","env:COPILOT_GITHUB_TOKEN","env:GH_TOKEN","env:GITHUB_TOKEN"]}}
"#,
    );
    write_private(
        home.join(".update_check"),
        br#"{"ts":4102444800,"behind":null,"rev":null,"ver":"0.20.0"}
"#,
    );
}

fn write_private(path: PathBuf, contents: &[u8]) {
    fs::write(&path, contents).expect("write isolated Hermes configuration");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .expect("private Hermes configuration");
}

/// Profile `[env]` lines that send every proxied Hermes request to `provider`.
fn proxy_profile_env(provider: &HermesProvider) -> String {
    let proxy = provider.proxy_url();
    PROXY_VARIABLES
        .iter()
        .map(|variable| format!("{variable} = \"{proxy}\"\n"))
        .chain(
            ["NO_PROXY", "no_proxy"]
                .iter()
                .map(|variable| format!("{variable} = \"{LOOPBACK_NO_PROXY}\"\n")),
        )
        .collect()
}

/// A pinned Hermes CLI command against the private Hermes home, with the
/// scenario's PATH and the loopback provider as its fail-closed proxy.
fn hermes_cli(
    harness: &Harness,
    hermes_bin: &Path,
    hermes_home: &Path,
    path: &str,
    provider: &HermesProvider,
) -> tokio::process::Command {
    let mut command = harness.env.tokio_command(hermes_bin);
    command.env("HERMES_HOME", hermes_home).env("PATH", path);
    for variable in PROXY_VARIABLES {
        command.env(variable, provider.proxy_url());
    }
    command
        .env("NO_PROXY", LOOPBACK_NO_PROXY)
        .env("no_proxy", LOOPBACK_NO_PROXY);
    command
}

/// The id of the one session in Hermes' own session store that holds the user
/// turn `prompt`, read through the pinned `hermes sessions export`.
///
/// This is the native conversation id as Hermes itself records it, so it is
/// independent of what the hook reporter told Pohunek.
async fn hermes_stored_session(mut export: tokio::process::Command, prompt: &str) -> String {
    export.args(["sessions", "export", "-", "--format", "jsonl"]);
    let output = guard("the pinned Hermes session export", export.output())
        .await
        .expect("run the pinned Hermes session export");
    assert!(
        output.status.success(),
        "Hermes session export succeeds: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let exported = String::from_utf8(output.stdout).expect("Hermes session export is UTF-8");
    let holding_turn: Vec<String> = exported
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("Hermes export line is JSON"))
        .filter(|session| {
            session["messages"].as_array().is_some_and(|messages| {
                messages
                    .iter()
                    .any(|message| message["role"] == "user" && message["content"] == prompt)
            })
        })
        .map(|session| {
            session["id"]
                .as_str()
                .expect("Hermes session id is a string")
                .to_owned()
        })
        .collect();
    let [id] = holding_turn.as_slice() else {
        panic!("exactly one stored Hermes session holds the turn: {holding_turn:?}");
    };
    id.clone()
}

/// Installs the built package through a throwaway signed catalog.
async fn install_official(
    harness: &Harness,
    key: &ed25519_dalek::SigningKey,
) -> (Vec<u8>, PackageDigest, PathBuf, PathBuf) {
    let (bytes, digest) = built_archive();
    let archive = write_archive(harness, "hermes.tar.zst", &bytes);
    let catalog = write_catalog(
        harness,
        key,
        1,
        vec![catalog_entry(PACKAGE_VERSION, &digest)],
    );

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

/// Requires the descriptor's version probe, as the daemon parses it from the
/// built package, to accept exactly the lock's `supported` range, and the lock
/// to name this package and runtime.
fn assert_supported_range_equals_the_lock(lock: &Value, locked_release: &str) {
    let (bytes, digest) = built_archive();
    let archive = read_archive(&bytes, &Limits::DEFAULT).expect("the archive reads");
    let definition = definition_from_archive(archive.entries(), &digest)
        .expect("the daemon accepts the package descriptor");
    let policy = definition
        .version_probe_policy()
        .expect("the descriptor declares a version probe");
    assert_eq!(
        definition.version_probe_parser().map(HandlerId::as_str),
        Some("semver-line-v1")
    );
    assert_eq!(lock["package"], PACKAGE_ID);
    assert_eq!(lock["runtime"], definition.runtime_id().as_str());
    assert_eq!(lock["supported"]["min"], policy.min().to_string());
    assert_eq!(lock["supported"]["below"], policy.below().to_string());
    let release =
        ProbeVersion::parse(locked_release).expect("the locked release is MAJOR.MINOR.PATCH");
    assert!(
        policy.supports(release),
        "the locked release lies in the supported range"
    );
    assert!(
        !policy.supports(policy.below()),
        "the first release above the range is unsupported"
    );
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
    assert_eq!(
        refused["err"]["code"], "package_archive_invalid",
        "{refused}"
    );
    selected_package(harness, digest).await;
}

/// Every file below `root`, keyed by path, with its bytes and permission bits.
fn tree(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, u32)> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries {
            let path = entry.expect("directory entry").path();
            let metadata = fs::symlink_metadata(&path).expect("entry metadata");
            if metadata.is_dir() {
                pending.push(path);
            } else {
                let mode = metadata.permissions().mode() & 0o7777;
                files.insert(path.clone(), (fs::read(&path).expect("read file"), mode));
            }
        }
    }
    files
}

/// Snapshot of every file a Hermes lifecycle action may change.
#[derive(Debug, PartialEq, Eq)]
struct DiskState {
    plugins: BTreeMap<PathBuf, (Vec<u8>, u32)>,
    policies: BTreeMap<PathBuf, (Vec<u8>, u32)>,
}

/// A controlled Hermes 0.20.0 installation for the explicit default profile.
struct HermesFixture {
    hermes_home: PathBuf,
    plugin_root: PathBuf,
    policy_dir: PathBuf,
    hermes_bin: PathBuf,
    pohunek_bin: PathBuf,
    enable_failure: PathBuf,
}

impl HermesFixture {
    fn new(harness: &Harness) -> Self {
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
                "case \"$*\" in\n--version) printf '%s\\n' 'Hermes Agent v0.20.0 (2026.8.3)' ;;\n'plugins list --json') if [ -d '{}' ]; then printf '[{{\"name\":\"pohunek\",\"status\":\"%s\",\"source\":\"user\"}}]\\n' \"$(/bin/cat '{}')\"; else printf '%s\\n' '[]'; fi ;;\n'plugins enable pohunek --no-allow-tool-override') if [ -e '{}' ]; then /bin/rm -f '{}'; exit 90; fi; printf '%s\\n' enabled > '{}' ;;\n'plugins disable pohunek') printf '%s\\n' disabled > '{}' ;;\n*) exit 90 ;;\nesac",
                plugin_root.display(),
                state_file.display(),
                enable_failure.display(),
                enable_failure.display(),
                state_file.display(),
                state_file.display(),
            ),
        )
        .expect("write controlled Hermes executable");
        Self {
            hermes_home,
            plugin_root,
            policy_dir: harness.env.state_home().join("pohunek/policies/hermes"),
            hermes_bin,
            pohunek_bin: bin_exe("pohunek"),
            enable_failure,
        }
    }

    /// Arguments of one Hermes lifecycle action for the default profile.
    fn args<'a>(&'a self, action: &'a str) -> Vec<&'a str> {
        vec![
            "integration",
            action,
            "--agent",
            "hermes",
            "--hermes-profile",
            "default",
            "--hermes-bin",
            path_str(&self.hermes_bin),
        ]
    }

    fn install_args(&self) -> Vec<&str> {
        let mut args = self.args("install");
        args.extend([
            "--pohunek-bin",
            path_str(&self.pohunek_bin),
            "--access-mode",
            "manage",
            "--allow-host",
            "local",
        ]);
        args
    }

    async fn install(&self, harness: &Harness) -> Value {
        let (code, installed) = harness.json(&self.install_args()).await;
        assert_eq!(code, 0, "Hermes integration install: {installed:#}");
        installed["ok"].clone()
    }

    async fn update(&self, harness: &Harness) -> Value {
        let (code, updated) = harness.json(&self.args("update")).await;
        assert_eq!(code, 0, "Hermes integration update: {updated:#}");
        updated["ok"].clone()
    }

    async fn status(&self, harness: &Harness) -> Value {
        let (code, status) = harness.json(&self.args("status")).await;
        assert_eq!(code, 0, "Hermes integration status: {status:#}");
        status["ok"].clone()
    }

    /// Runs doctor, whose exit code reports health, and returns its envelope.
    async fn doctor(&self, harness: &Harness) -> Value {
        let (code, doctor) = harness.json(&self.args("doctor")).await;
        let healthy = doctor["ok"]["doctor"]["ok"] == true;
        assert_eq!(code, i32::from(!healthy), "doctor exit code: {doctor:#}");
        doctor["ok"].clone()
    }

    /// Runs `args`, which must fail with `code`, and returns the error object.
    async fn refused(&self, harness: &Harness, args: &[&str], code: &str, context: &str) -> Value {
        let (exit, refused) = harness.json(args).await;
        assert_eq!(exit, 1, "{context}: {refused:#}");
        assert_eq!(refused["err"]["code"], code, "{context}: {refused:#}");
        refused["err"].clone()
    }

    /// Refuses install and update with `code`, asserts nothing changed on disk,
    /// and returns the install error object.
    async fn refuses_install_and_update(
        &self,
        harness: &Harness,
        code: &str,
        context: &str,
    ) -> Value {
        let before = self.disk();
        let install = self.install_args();
        let refused = self.refused(harness, &install, code, context).await;
        assert_eq!(self.disk(), before, "{context}: install changed nothing");
        let update = self.args("update");
        let refused_update = self.refused(harness, &update, code, context).await;
        assert_eq!(self.disk(), before, "{context}: update changed nothing");
        assert_eq!(refused["recover"], refused_update["recover"], "{context}");
        refused
    }

    /// Refuses an install over an unrelated plugin directory and preserves it.
    async fn refuses_unowned_collision(&self, harness: &Harness) {
        fs::create_dir_all(&self.plugin_root).expect("create colliding plugin directory");
        for directory in [
            self.plugin_root.parent().expect("operator parent"),
            self.plugin_root.as_path(),
        ] {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
                .expect("make plugin directory private");
        }
        let owner_file = self.plugin_root.join("owner-file");
        fs::write(&owner_file, b"preserve").expect("write unrelated plugin data");
        fs::set_permissions(&owner_file, fs::Permissions::from_mode(0o600))
            .expect("make collision file private");
        let install = self.install_args();
        self.refused(
            harness,
            &install,
            "hermes_plugin_collision",
            "unowned plugin collision",
        )
        .await;
        assert_eq!(
            fs::read(&owner_file).expect("preserved collision"),
            b"preserve"
        );
        fs::remove_dir_all(&self.plugin_root).expect("remove fixture collision");
    }

    /// A failed Hermes enable during update restores the previous files and state.
    async fn assert_failed_enable_rolls_back(&self, harness: &Harness) {
        let before = self.disk();
        fs::write(&self.enable_failure, b"fail").expect("arm one Hermes enable failure");
        let mut update = self.args("update");
        update.extend(["--access-mode", "full"]);
        self.refused(
            harness,
            &update,
            "hermes_command_failed",
            "a failed Hermes enable rolls the update back",
        )
        .await;
        assert!(
            !self.enable_failure.exists(),
            "the update attempted exactly the failing enable"
        );
        assert_eq!(
            self.disk(),
            before,
            "the rolled back update changed nothing"
        );
        assert_eq!(
            self.status(harness).await["enabled"],
            true,
            "the rollback restores the enabled plugin"
        );
    }

    fn disk(&self) -> DiskState {
        DiskState {
            plugins: tree(&self.hermes_home.join("plugins")),
            policies: tree(&self.policy_dir),
        }
    }

    fn marker(&self) -> Value {
        serde_json::from_slice(
            &fs::read(self.plugin_root.join(".pohunek-owned.json")).expect("read marker"),
        )
        .expect("parse marker")
    }

    /// The canonical path of the only policy file, which the installer derives
    /// from the state home.
    fn policy_path(&self) -> PathBuf {
        let policies: Vec<_> = fs::read_dir(&self.policy_dir)
            .expect("read policy directory")
            .map(|entry| entry.expect("policy entry").path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect();
        assert_eq!(policies.len(), 1, "exactly one installed policy");
        fs::canonicalize(&policies[0]).expect("canonical policy path")
    }
}

/// One doctor check as `(status, recovery_hint)`.
fn check<'a>(envelope: &'a Value, code: &str) -> (&'a str, &'a str) {
    let check = envelope["doctor"]["checks"]
        .as_array()
        .expect("doctor checks")
        .iter()
        .find(|check| check["code"] == code)
        .unwrap_or_else(|| panic!("doctor check {code}: {envelope:#}"));
    (
        check["status"].as_str().expect("check status"),
        check["recovery_hint"].as_str().expect("check hint"),
    )
}

/// Asserts the source findings that status and doctor report for one state.
async fn assert_source_findings(
    harness: &Harness,
    hermes: &HermesFixture,
    missing: bool,
    error: Option<&str>,
    context: &str,
) {
    let status = hermes.status(harness).await;
    if error.is_some() {
        assert_eq!(
            status["outdated"], false,
            "{context}: source loss is not outdated"
        );
    }
    assert_eq!(
        status["package_source_missing"], missing,
        "{context}: {status:#}"
    );
    assert_eq!(
        status["package_source_error"],
        error.map_or(Value::Null, Value::from),
        "{context}: {status:#}"
    );
    let doctor = hermes.doctor(harness).await;
    assert_eq!(
        doctor["package_source_missing"], missing,
        "{context}: {doctor:#}"
    );
    assert_eq!(
        doctor["package_source_error"],
        error.map_or(Value::Null, Value::from),
        "{context}: {doctor:#}"
    );
    let (source, _) = check(&doctor, "asset_source");
    assert_eq!(
        source,
        if error.is_some() { "fail" } else { "pass" },
        "{context}: asset_source fails exactly when a source error exists: {doctor:#}"
    );
    if error.is_some() {
        assert_eq!(
            doctor["outdated"], false,
            "{context}: source loss is not outdated"
        );
        assert_ne!(
            check(&doctor, "asset_current").0,
            "fail",
            "{context}: doctor does not offer an update that cannot succeed: {doctor:#}"
        );
        assert_eq!(doctor["doctor"]["ok"], false, "{context}");
    }
}

/// Doctor's `asset_source` hint is the recovery install and update report.
async fn assert_source_hint(harness: &Harness, hermes: &HermesFixture, code: &str) {
    let doctor = hermes.doctor(harness).await;
    let install = hermes.install_args();
    let refused = hermes
        .refused(harness, &install, code, "install refusal for the hint")
        .await;
    assert_eq!(
        check(&doctor, "asset_source").1,
        refused["recover"].as_str().expect("recovery hint"),
        "{doctor:#}"
    );
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
    let locked_release = lock["upstream"]["release"]
        .as_str()
        .expect("locked release");
    assert_eq!(locked_release, "0.20.0");
    assert_supported_range_equals_the_lock(&lock, locked_release);
    let locked_banner = lock["cli_checks"]
        .as_array()
        .expect("CLI checks")
        .iter()
        .find(|check| check["id"] == "version")
        .and_then(|check| check["required_text"][0].as_str())
        .expect("the lock has a required version banner");

    let key = test_key();
    let harness = trusted_harness(&key).await;
    let (bytes, digest, archive, catalog) = install_official(&harness, &key).await;
    selected_package(&harness, &digest).await;
    assert_supported_boundary(&harness, &digest, locked_release, locked_banner).await;
    assert_failed_installs_preserve_selection(&harness, &digest, &bytes, &archive, &catalog).await;
    harness.stop().await;
}

/// Runs one pinned Hermes turn through the selected official package and the public PTY API.
#[tokio::test]
#[ignore = "needs provisioned Hermes; set POHUNEK_HERMES_E2E=1 and POHUNEK_HERMES_BIN, then use --ignored"]
#[expect(
    clippy::too_many_lines,
    reason = "one end-to-end scenario asserts the package-to-screen boundary"
)]
async fn a_real_hermes_turn_uses_the_selected_package_and_public_session_screen() {
    let _process_env = ProcessEnv::lock();
    let hermes_bin = require_real_hermes();
    let key = test_key();
    let harness = trusted_harness(&key).await;
    let (_, digest, _, _) = install_official(&harness, &key).await;
    selected_package(&harness, &digest).await;

    let hermes_home = harness.env.root().join("hermes-home");
    let provider = HermesProvider::start(REAL_TURN_RESPONSE);
    configure_real_hermes(&hermes_home, &provider);
    let probe_dir = harness.env.root().join("python-probe");
    fs::create_dir_all(&probe_dir).expect("create private Python probe directory");
    let hermes_operator_root = hermes_home.join("plugins/operators");
    let child_bin = harness.env.root().join("provider-child-bin");
    fs::create_dir_all(&child_bin).expect("create provider child fixture");
    let hermes_python = hermes_bin
        .parent()
        .expect("Hermes bin directory")
        .join("python");
    symlink(
        hermes_bin
            .parent()
            .expect("Hermes bin directory")
            .parent()
            .expect("Hermes virtual environment")
            .join("lib"),
        harness.env.root().join("lib"),
    )
    .expect("make the provisioned Python libraries available beside the child interpreter");
    let mut python_probe = tokio::process::Command::new(&hermes_python);
    python_probe
        .env_clear()
        .args(["-c", "import site; print(site.getsitepackages()[0])"]);
    let python_probe = guard(
        "the provisioned Hermes Python site-packages query",
        python_probe.output(),
    )
    .await
    .expect("query provisioned Hermes Python site-packages");
    assert!(
        python_probe.status.success(),
        "query provisioned Hermes Python site-packages"
    );
    let python_site_packages = PathBuf::from(
        String::from_utf8(python_probe.stdout)
            .expect("Hermes Python site-packages path is UTF-8")
            .trim(),
    );
    assert!(python_site_packages.is_absolute());
    let child_hermes = child_bin.join("hermes");
    fs::copy(&hermes_python, &child_hermes)
        .expect("copy the Hermes Python interpreter as a provider-named executable");
    fs::copy(
        hermes_bin
            .parent()
            .expect("Hermes bin directory")
            .parent()
            .expect("Hermes virtual environment")
            .join("pyvenv.cfg"),
        child_bin.join("pyvenv.cfg"),
    )
    .expect("copy the provisioned Hermes virtual-environment configuration");
    fs::set_permissions(&child_hermes, fs::Permissions::from_mode(0o700))
        .expect("make provider-named child executable private");
    let barrier = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind the test-owned startup barrier");
    let barrier_address = barrier.local_addr().expect("read startup barrier address");
    let probe_marker = harness.env.root().join("python-probe-loaded");
    let one_shot_marker = harness.env.root().join("python-probe-consumed");
    let child_marker = harness.env.root().join("provider-child-status");
    let child_script = harness.env.root().join("provider-child.py");
    fs::write(
        &child_script,
        format!(
            r"import os, sys
def actual_executable():
    if sys.platform == 'linux':
        return os.readlink('/proc/self/exe')
    return sys.executable
with open({child_marker:?}, 'w', encoding='ascii') as status:
    status.write('started\n')
try:
    from pohunek._pohunek_reporter import Reporter
    import socket
    reporter = Reporter('hermes')
    original_send = reporter._send
    identity_responses = []
    def capture_worker_identity(endpoint, message):
        response = original_send(endpoint, message)
        if endpoint == reporter._worker_socket and message.get('type') == 'identity_report':
            identity_responses.append(response)
        return response
    reporter._send = capture_worker_identity
    with open({child_marker:?}, 'a', encoding='ascii') as status:
        status.write('reporter_active=' + str(reporter.active) + '\n')
        status.write('executable=' + os.path.basename(actual_executable()) + '\n')
    reporter.identity('s4b-provider-child-spoof')
    claim_response = identity_responses[-1]
    barrier = socket.create_connection(({host:?}, {port}), timeout={STARTUP_BARRIER_SECONDS})
    barrier.sendall((str(os.getpid()) + ' ' + str(reporter.active) + ' ' + str(reporter.failures) + ' ' + os.path.basename(actual_executable()) + ' ' + str(os.getppid()) + ' ' + str(claim_response.get('launch_identity_accepted')) + ' ' + str(claim_response.get('launch_identity_status')) + '\n').encode('ascii'))
    barrier.recv(1)
    barrier.close()
except BaseException as error:
    with open({child_marker:?}, 'a', encoding='ascii') as status:
        status.write('error=' + type(error).__name__ + '\n')
    raise
",
            child_marker = child_marker.to_string_lossy(),
            host = barrier_address.ip().to_string(),
            port = barrier_address.port(),
        ),
    )
    .expect("write test-owned Python provider child");
    let sitecustomize = format!(
        r"import os, subprocess, sys
open({probe_marker:?}, 'a', encoding='ascii').write('argv0=' + os.path.basename(sys.argv[0]) + ';version_flag=' + str(any(arg in ('--version', 'version', '-V', '--help', '-h') for arg in sys.argv[1:])) + ';argc=' + str(len(sys.argv)) + '\n')
if os.environ.get('HERMES_TEST_CHILD_PROBE') == '1' and not any(arg in ('--version', 'version', '-V', '--help', '-h') for arg in sys.argv[1:]):
    try:
        marker_fd = os.open({one_shot_marker:?}, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    except FileExistsError:
        marker_fd = None
    if marker_fd is not None:
        os.close(marker_fd)
        try:
            child = subprocess.Popen([{child_hermes:?}, {child_script:?}], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, close_fds=True)
            open({child_marker:?}, 'a', encoding='ascii').write('spawned\n')
            child.wait(timeout={STARTUP_BARRIER_SECONDS})
        except BaseException as error:
            open({child_marker:?}, 'a', encoding='ascii').write('parent_error=' + type(error).__name__ + '\n')
            raise
",
        child_hermes = child_hermes.to_string_lossy(),
        child_script = child_script.to_string_lossy(),
        probe_marker = probe_marker.to_string_lossy(),
        one_shot_marker = one_shot_marker.to_string_lossy(),
        child_marker = child_marker.to_string_lossy(),
    );
    fs::write(probe_dir.join("sitecustomize.py"), sitecustomize)
        .expect("write test-owned Python startup probe");
    let path = format!(
        "{}:/usr/bin:/bin",
        hermes_bin.parent().expect("Hermes bin directory").display()
    );
    harness.profile(
        PROFILE,
        &format!(
            "base = \"hermes\"\npackage = \"{PACKAGE_ID}\"\ndigest = \"{}\"\nprogram = \"{}\"\n[env]\nHERMES_HOME = \"{}\"\nPATH = \"{}\"\nPYTHONPATH = \"{}:{}:{}\"\nHERMES_TEST_CHILD_PROBE = \"1\"\nDBUS_SESSION_BUS_ADDRESS = \"unix:path={}/unreachable-dbus\"\n{}",
            digest.as_str(),
            path_str(&hermes_bin),
            path_str(&hermes_home),
            path,
            path_str(&probe_dir),
            path_str(&hermes_operator_root),
            path_str(&python_site_packages),
            path_str(harness.env.root()),
            proxy_profile_env(&provider),
        ),
    );

    inspect_profile_version(&harness, "0.20.0", true).await;

    let install = harness
        .json(&[
            "integration",
            "install",
            "--agent",
            "hermes",
            "--hermes-home",
            path_str(&hermes_home),
            "--hermes-bin",
            path_str(&hermes_bin),
            "--pohunek-bin",
            path_str(&bin_exe("pohunek")),
            "--access-mode",
            "manage",
            "--allow-host",
            "local",
        ])
        .await;
    assert_eq!(
        install.0, 0,
        "install package operator plugin: {}",
        install.1
    );
    let plugin_root = hermes_operator_root.join("pohunek");
    let marker: Value = serde_json::from_slice(
        &fs::read(plugin_root.join(".pohunek-owned.json")).expect("package marker"),
    )
    .expect("package marker JSON");
    assert_eq!(marker["source"], "package");
    assert_eq!(marker["package_id"], PACKAGE_ID);
    assert_eq!(
        marker["package_digest"],
        digest.as_str().trim_start_matches("sha256:")
    );
    let installed_skill = fs::read(plugin_root.join(SKILL_PATH)).expect("read installed skill");
    assert_eq!(
        installed_skill,
        entry(&package_entries(), &format!("operator/{SKILL_PATH}")),
        "the real Hermes plugin receives the skill of the selected package"
    );
    assert_ne!(
        installed_skill,
        embedded_operator_file(SKILL_PATH),
        "the embedded skill is not installed for the real Hermes"
    );
    let mut plugin_list = hermes_cli(&harness, &hermes_bin, &hermes_home, &path, &provider);
    plugin_list.args(["plugins", "list", "--json"]);
    let plugin_list = guard("the pinned Hermes plugin list", plugin_list.output())
        .await
        .expect("list installed Hermes plugins");
    assert!(
        plugin_list.status.success(),
        "Hermes plugin list succeeds: {}",
        String::from_utf8_lossy(&plugin_list.stderr)
    );
    let plugins: Value = serde_json::from_slice(&plugin_list.stdout).expect("Hermes plugin list");
    assert!(
        plugins.as_array().is_some_and(|entries| {
            entries
                .iter()
                .any(|plugin| plugin["name"] == "pohunek" && plugin["status"] == "enabled")
        }),
        "the package-sourced Pohunek plugin is enabled: {plugins}"
    );

    let cwd = harness.env.cwd().to_path_buf();
    let mut launch = harness.env.tokio_command(bin_exe("pohunek"));
    launch
        .args([
            "session",
            "new",
            "--agent",
            PROFILE,
            "--cwd",
            path_str(&cwd),
            "--name",
            "hermes-package-turn",
            "--json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut launch = launch.spawn().expect("start Hermes session launch");
    let Ok(accepted) = tokio::time::timeout(
        std::time::Duration::from_secs(STARTUP_BARRIER_SECONDS),
        barrier.accept(),
    )
    .await
    else {
        // A launch that already exited cannot be killed; its output is still collected.
        let _ = launch.kill().await;
        let output = guard("the stopped session launch", launch.wait_with_output())
            .await
            .expect("collect failed launch");
        panic!(
            "Python startup probe did not reach its barrier; probe={}, launch_exit={:?}, \
             launch_stdout={}, launch_stderr={}, child_status={}, provider: {}",
            fs::read_to_string(&probe_marker).unwrap_or_else(|_| "not imported".to_owned()),
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            fs::read_to_string(&child_marker).unwrap_or_else(|_| "not started".to_owned()),
            provider.diagnostics(),
        );
    };
    let (mut probe, _) = accepted.expect("accept startup probe");
    let child_pid = guard("the provider child claim line", async {
        let mut line = Vec::new();
        loop {
            let mut byte = [0_u8; 1];
            probe
                .read_exact(&mut byte)
                .await
                .expect("read provider child pid");
            if byte[0] == b'\n' {
                break line;
            }
            line.push(byte[0]);
        }
    })
    .await;
    let child_claim = String::from_utf8(child_pid).expect("provider child claim text");
    let mut child_claim = child_claim.split_ascii_whitespace();
    let child_pid = child_claim.next().expect("provider child pid");
    assert!(child_pid.bytes().all(|byte| byte.is_ascii_digit()));
    let reporter_active = child_claim.next().expect("child reporter active state");
    let reporter_failures = child_claim.next().expect("child reporter failure count");
    let child_executable = child_claim.next().expect("child executable basename");
    let child_parent_pid = child_claim.next().expect("child parent pid");
    let child_launch_accepted = child_claim.next().expect("worker launch acceptance");
    let child_launch_status = child_claim.next().expect("worker launch status");
    assert_eq!(
        reporter_active, "True",
        "child reporter has the worker identity context"
    );
    assert_eq!(
        reporter_failures, "0",
        "child identity report was delivered"
    );
    assert_eq!(
        child_executable, "hermes",
        "child executable has provider basename"
    );
    assert_eq!(
        child_launch_accepted, "False",
        "the worker rejects the provider-named descendant as launch identity"
    );
    assert_eq!(child_launch_status, "rejected");
    assert!(
        child_claim.next().is_none(),
        "child claim has no extra fields"
    );
    let listed = wait_until("the paused Hermes session registration", || async {
        provider.assert_healthy();
        let (code, sessions) = harness.json(&["session", "list"]).await;
        assert_eq!(
            code, 0,
            "list sessions while Hermes startup is paused: {sessions}"
        );
        sessions["ok"]
            .as_array()?
            .iter()
            .find(|session| session["name"] == "hermes-package-turn")
            .and_then(|session| session["id"].as_str())
            .map(str::to_owned)
    })
    .await;
    let (code, paused_inspect) = harness.json(&["session", "inspect", &listed]).await;
    assert_eq!(code, 0, "inspect paused Hermes session: {paused_inspect}");
    assert!(
        paused_inspect["ok"]["native_session_id"].is_null(),
        "the provider-named child claim must not become durable before the root reports"
    );
    assert_eq!(
        child_parent_pid,
        paused_inspect["ok"]["pid"]
            .as_u64()
            .expect("PTY root pid")
            .to_string(),
        "the provider-named reporter is a direct child of the PTY root"
    );
    assert_ne!(child_pid, child_parent_pid);
    let journal = worker_journal(harness.env.root(), &listed);
    assert!(
        journal["launch_identity"].is_null(),
        "the durable worker journal does not accept the provider-named child"
    );
    let child = child_pid.parse::<u32>().expect("child pid");
    assert!(
        journal["pending_launch_claims"]
            .as_array()
            .is_none_or(|claims| claims
                .iter()
                .all(|claim| claim["identity"]["process"]["pid"] != child)),
        "the frozen-root rule refuses the child's claim without keeping it pending: {}",
        journal["pending_launch_claims"]
    );
    probe
        .write_all(&[1])
        .await
        .expect("release the test-owned startup barrier");
    drop(probe);
    let launched_output = guard("the Hermes session launch", launch.wait_with_output())
        .await
        .expect("finish Hermes session launch");
    let code = launched_output
        .status
        .code()
        .expect("session launch exit code");
    let launched: Value = serde_json::from_slice(&launched_output.stdout).unwrap_or_else(|error| {
        panic!(
            "session launch JSON ({error}): {}",
            String::from_utf8_lossy(&launched_output.stdout)
        )
    });
    assert_eq!(
        code,
        0,
        "launch package-pinned Hermes: {launched}; sessions: {}; provider: {}",
        harness.json(&["session", "list"]).await.1,
        provider.diagnostics()
    );
    let session_id = launched["ok"]["id"]
        .as_str()
        .expect("session id")
        .to_owned();
    assert_eq!(launched["ok"]["agent_base"], "hermes");

    let (code, initial_screen) = harness.json(&["session", "screen", &session_id]).await;
    assert_eq!(code, 0, "read initial Hermes screen: {initial_screen}");
    let (code, initial_inspect) = harness.json(&["session", "inspect", &session_id]).await;
    assert_eq!(code, 0, "inspect initial Hermes session: {initial_inspect}");
    assert_eq!(initial_inspect["ok"]["agent_base"], "hermes");
    wait_until("the real Hermes welcome screen", || async {
        provider.assert_healthy();
        let (code, screen) = harness.json(&["session", "screen", &session_id]).await;
        assert_eq!(code, 0, "read Hermes screen: {screen}");
        let visible = screen["ok"]["visible_lines"]
            .as_array()
            .expect("visible Hermes lines");
        visible
            .iter()
            .filter_map(Value::as_str)
            .any(|line| line.contains("Welcome to Hermes Agent!"))
            .then_some(())
    })
    .await;

    let (code, sent) = harness
        .json(&["session", "input", &session_id, REAL_TURN_PROMPT])
        .await;
    assert_eq!(code, 0, "submit safe Hermes prompt: {sent}");
    wait_until("the model response in the public Hermes screen", || async {
        provider.assert_healthy();
        let (code, screen) = harness.json(&["session", "screen", &session_id]).await;
        assert_eq!(code, 0, "read Hermes response screen: {screen}");
        let visible = screen["ok"]["visible_lines"]
            .as_array()
            .expect("visible Hermes lines");
        visible
            .iter()
            .filter_map(Value::as_str)
            .any(|line| line.contains(REAL_TURN_RESPONSE))
            .then_some(())
    })
    .await;
    wait_until("the real Hermes session to become idle", || async {
        provider.assert_healthy();
        let (code, inspected) = harness.json(&["session", "inspect", &session_id]).await;
        assert_eq!(code, 0, "inspect Hermes session: {inspected}");
        (inspected["ok"]["activity"] == "idle").then_some(())
    })
    .await;
    provider.assert_turn(REAL_TURN_PROMPT);
    let native_id = hermes_stored_session(
        hermes_cli(&harness, &hermes_bin, &hermes_home, &path, &provider),
        REAL_TURN_PROMPT,
    )
    .await;
    let (code, reported) = harness.json(&["session", "inspect", &session_id]).await;
    assert_eq!(code, 0, "inspect reported Hermes identity: {reported}");
    assert_eq!(
        reported["ok"]["native_session_id"], native_id,
        "the installed Hermes reporter supplies the conversation id Hermes stored the turn under"
    );
    assert_ne!(
        native_id, "s4b-provider-child-spoof",
        "the descendant identity claim must not replace the real Hermes root identity"
    );
    let journal = worker_journal(harness.env.root(), &session_id);
    let launch_identity = &journal["launch_identity"];
    assert_eq!(
        launch_identity["provider"], "hermes",
        "the worker accepts the Hermes root's launch claim: {}",
        journal["pending_launch_claims"]
    );
    assert_eq!(
        launch_identity["process"]["pid"], reported["ok"]["pid"],
        "the accepted launch process is the PTY root running the frozen entrypoint"
    );
    assert_eq!(
        launch_identity["native_reference"], native_id,
        "the hook-reported native reference is the accepted launch reference"
    );

    let (code, stopped) = harness.json(&["session", "stop", &session_id]).await;
    assert_eq!(code, 0, "stop Hermes session: {stopped}");
    let (code, resumed) = harness.json(&["session", "resume", &session_id]).await;
    assert_eq!(
        code, 0,
        "resume the reported Hermes conversation: {resumed}"
    );
    wait_until("the resumed Hermes session to become idle", || async {
        provider.assert_healthy();
        let (code, inspected) = harness.json(&["session", "inspect", &session_id]).await;
        assert_eq!(code, 0, "inspect resumed Hermes session: {inspected}");
        (inspected["ok"]["activity"] == "idle").then_some(())
    })
    .await;
    let (code, resumed_inspect) = harness.json(&["session", "inspect", &session_id]).await;
    assert_eq!(
        code, 0,
        "inspect resumed Hermes identity: {resumed_inspect}"
    );
    assert_eq!(resumed_inspect["ok"]["native_session_id"], native_id);
    assert_eq!(
        hermes_stored_session(
            hermes_cli(&harness, &hermes_bin, &hermes_home, &path, &provider),
            REAL_TURN_PROMPT,
        )
        .await,
        native_id,
        "resume continues the stored Hermes conversation"
    );
    provider.assert_turn(REAL_TURN_PROMPT);
    let (code, resumed_screen) = harness.json(&["session", "screen", &session_id]).await;
    assert_eq!(code, 0, "read resumed Hermes screen: {resumed_screen}");
    assert!(resumed_screen["ok"]["visible_lines"]
        .as_array()
        .expect("resumed Hermes visible lines")
        .iter()
        .filter_map(Value::as_str)
        .any(|line| line.contains(REAL_TURN_RESPONSE)));

    let (code, removed) = harness.json(&["session", "rm", &session_id]).await;
    assert_eq!(code, 0, "remove Hermes session and worker: {removed}");
    let (code, missing) = harness.json(&["session", "inspect", &session_id]).await;
    assert_eq!(code, 1, "removed Hermes session is absent: {missing}");
    harness.stop().await;
}

/// A package-backed install writes the package bytes, provenance, and policy binding.
#[tokio::test]
async fn package_backed_install_writes_the_selected_package_bytes() {
    let key = test_key();
    let harness = trusted_harness(&key).await;
    let (_, digest, _, _) = install_official(&harness, &key).await;
    let hermes = HermesFixture::new(&harness);
    hermes.refuses_unowned_collision(&harness).await;

    hermes.install(&harness).await;
    let package = package_entries();
    let packaged_skill = entry(&package, &format!("operator/{SKILL_PATH}"));
    let installed_skill =
        fs::read(hermes.plugin_root.join(SKILL_PATH)).expect("read installed skill");
    assert_eq!(
        installed_skill, packaged_skill,
        "the package supplies the skill"
    );
    assert_ne!(
        installed_skill,
        embedded_operator_file(SKILL_PATH),
        "the embedded skill is not installed"
    );
    for path in OPERATOR_PAYLOAD
        .iter()
        .filter(|path| !matches!(**path, "__init__.py" | SKILL_PATH))
    {
        assert_eq!(
            fs::read(hermes.plugin_root.join(path)).expect("read installed payload"),
            entry(&package, &format!("operator/{path}")),
            "{path} comes from the package"
        );
    }

    let marker = hermes.marker();
    assert_eq!(marker["version"], 3);
    assert_eq!(marker["source"], "package");
    assert_eq!(marker["package_id"], PACKAGE_ID);
    assert_eq!(
        marker["package_digest"],
        digest.as_str().trim_start_matches("sha256:")
    );
    assert_eq!(
        marker["operator_sha256"],
        sha256_hex(entry(&package, "operator.toml")),
        "the marker records the package sidecar"
    );
    assert_eq!(
        marker["assets"][SKILL_PATH],
        sha256_hex(packaged_skill),
        "the marker checksums the installed package skill"
    );

    let recorded_home = marker["hermes_home"].as_str().expect("marker Hermes home");
    assert_eq!(
        Path::new(recorded_home),
        fs::canonicalize(&hermes.hermes_home).expect("canonical Hermes home"),
        "the marker records the selected Hermes home"
    );
    // The installer keys the policy by the target home below the state home.
    let expected_policy = fs::canonicalize(&hermes.policy_dir)
        .expect("canonical policy directory")
        .join(format!("{}.json", sha256_hex(recorded_home.as_bytes())));
    assert_eq!(hermes.policy_path(), expected_policy);
    assert_eq!(marker["policy_path"], path_str(&expected_policy));
    let init = fs::read_to_string(hermes.plugin_root.join("__init__.py")).expect("read init");
    assert!(!init.contains(POLICY_TOKEN));
    assert!(
        init.contains(&format!(
            "POLICY_PATH = {}",
            serde_json::to_string(path_str(&expected_policy)).expect("policy literal")
        )),
        "the policy token binds the state-home policy path"
    );
    assert_source_findings(&harness, &hermes, false, None, "fresh package install").await;

    let marker_bytes = fs::read(hermes.plugin_root.join(".pohunek-owned.json")).expect("marker");
    hermes.install(&harness).await;
    assert_eq!(
        fs::read(hermes.plugin_root.join(".pohunek-owned.json")).expect("marker"),
        marker_bytes,
        "a repeated install is idempotent"
    );

    hermes.assert_failed_enable_rolls_back(&harness).await;
    harness.stop().await;
}

/// No usable selection fails closed without claiming a recorded source was lost.
#[tokio::test]
async fn an_unusable_selection_without_an_install_is_a_typed_source_error() {
    let key = test_key();
    let harness = trusted_harness(&key).await;
    let (_, digest, _, _) = install_official(&harness, &key).await;
    let hermes = HermesFixture::new(&harness);

    let mut alternate = package_entries();
    set_version(&mut alternate, "1.0.1");
    let (bytes, alternate_digest) = archive_of(&alternate);
    let archive = write_archive(&harness, "hermes-1.0.1.tar.zst", &bytes);
    let catalog = write_catalog(
        &harness,
        &key,
        2,
        vec![catalog_entry("1.0.1", &alternate_digest)],
    );
    update_package(&harness, &archive, &catalog, &alternate_digest).await;
    let (code, removed) = harness
        .json(&[
            "plugin",
            "uninstall",
            PACKAGE_ID,
            "--digest",
            alternate_digest.as_str(),
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "remove selected alternate version: {removed:#}");
    let (code, listed) = harness.json(&["plugin", "list"]).await;
    assert_eq!(code, 0, "list unselected Hermes package: {listed:#}");
    let remaining = listed["ok"]["packages"]
        .as_array()
        .expect("package list")
        .iter()
        .find(|package| package["package"]["id"] == PACKAGE_ID)
        .expect("original package remains installed");
    assert_eq!(remaining["selected"], false);

    assert_source_findings(
        &harness,
        &hermes,
        false,
        Some("hermes_package_not_selected"),
        "unselected package without an install",
    )
    .await;
    let refused = hermes
        .refuses_install_and_update(
            &harness,
            "hermes_package_not_selected",
            "unselected package",
        )
        .await;
    assert_eq!(refused["class"], "configuration", "{refused:#}");
    assert_source_hint(&harness, &hermes, "hermes_package_not_selected").await;
    assert!(!hermes.plugin_root.exists());

    select_package(&harness, &digest).await;
    let registry_root = harness.plugins.clone();
    fs::set_permissions(&registry_root, fs::Permissions::from_mode(0o770))
        .expect("make the registry root group-writable");
    assert_source_findings(
        &harness,
        &hermes,
        false,
        Some("hermes_package_registry_unreadable"),
        "unsafe registry root",
    )
    .await;
    let refused = hermes
        .refuses_install_and_update(
            &harness,
            "hermes_package_registry_unreadable",
            "unsafe registry root",
        )
        .await;
    assert_eq!(refused["class"], "runtime", "{refused:#}");
    assert_source_hint(&harness, &hermes, "hermes_package_registry_unreadable").await;
    fs::set_permissions(&registry_root, fs::Permissions::from_mode(0o700))
        .expect("restore the registry root mode");

    hermes.install(&harness).await;
    assert_eq!(hermes.marker()["source"], "package");
    harness.stop().await;
}

/// Tampered, missing, and extra files in the installed package root fail closed
/// as integrity errors.
async fn assert_damaged_roots_are_refused(
    harness: &Harness,
    hermes: &HermesFixture,
    digest: &PackageDigest,
) {
    let operator_root = harness.root_of(digest).join("files/operator");
    for (path, damage) in [
        ("tools.py", Some(b"tampered".as_slice())),
        ("redact.py", None),
        ("extra.py", Some(b"extra".as_slice())),
    ] {
        let file = operator_root.join(path);
        let original = fs::read(&file).ok();
        match damage {
            Some(bytes) => {
                fs::write(&file, bytes).expect("write package file");
                fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
                    .expect("private package file");
            }
            None => fs::remove_file(&file).expect("remove package file"),
        }
        let context = format!("damaged package root ({path})");
        assert_source_findings(
            harness,
            hermes,
            true,
            Some("hermes_invalid_asset"),
            &context,
        )
        .await;
        hermes
            .refuses_install_and_update(harness, "hermes_invalid_asset", &context)
            .await;
        match original {
            Some(bytes) => {
                fs::write(&file, bytes).expect("restore package file");
                fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
                    .expect("restore package mode");
            }
            None => fs::remove_file(&file).expect("remove extra package file"),
        }
    }
}

/// Losing the recorded package keeps status informative, refuses update, and allows uninstall.
#[tokio::test]
async fn losing_the_recorded_package_refuses_update_without_mutation() {
    let key = test_key();
    let harness = trusted_harness(&key).await;
    let (_, digest, _, _) = install_official(&harness, &key).await;
    let hermes = HermesFixture::new(&harness);
    hermes.install(&harness).await;

    let (code, disabled) = harness
        .json(&["plugin", "disable", PACKAGE_ID, "--digest", digest.as_str()])
        .await;
    assert_eq!(code, 0, "disable Hermes package: {disabled:#}");
    hermes
        .refuses_install_and_update(&harness, "hermes_package_not_selected", "disabled package")
        .await;
    assert_source_findings(
        &harness,
        &hermes,
        true,
        Some("hermes_package_not_selected"),
        "disabled package",
    )
    .await;
    assert_source_hint(&harness, &hermes, "hermes_package_not_selected").await;
    let tools = hermes.plugin_root.join("tools.py");
    let tools_bytes = fs::read(&tools).expect("installed tools");
    let mut edited = tools_bytes.clone();
    edited.extend_from_slice(b"\n# owner edit\n");
    fs::write(&tools, &edited).expect("edit installed tools");
    let doctor = hermes.doctor(&harness).await;
    let (integrity, hint) = check(&doctor, "asset_integrity");
    assert_eq!(integrity, "fail", "{doctor:#}");
    assert!(
        hint.starts_with("restore the Hermes operator source"),
        "a modified install without a usable source is not told to update first: {doctor:#}"
    );
    fs::write(&tools, &tools_bytes).expect("restore installed tools");
    let (code, enabled) = harness
        .json(&["plugin", "enable", PACKAGE_ID, "--digest", digest.as_str()])
        .await;
    assert_eq!(code, 0, "restore Hermes package: {enabled:#}");
    assert_source_findings(&harness, &hermes, false, None, "re-enabled package").await;

    assert_damaged_roots_are_refused(&harness, &hermes, &digest).await;
    assert_source_findings(&harness, &hermes, false, None, "repaired package root").await;

    let (code, removed) = harness
        .json(&[
            "plugin",
            "uninstall",
            PACKAGE_ID,
            "--digest",
            digest.as_str(),
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "remove package source: {removed:#}");
    assert_source_findings(
        &harness,
        &hermes,
        true,
        Some("hermes_package_source_missing"),
        "removed package",
    )
    .await;
    let marker_before = fs::read(hermes.plugin_root.join(".pohunek-owned.json")).expect("marker");
    let policy_before = fs::read(hermes.policy_path()).expect("policy");
    let before = hermes.disk();
    let mut update = hermes.args("update");
    update.extend(["--access-mode", "full"]);
    hermes
        .refused(
            &harness,
            &update,
            "hermes_package_source_missing",
            "update cannot switch a package install to embedded assets",
        )
        .await;
    assert_eq!(
        hermes.disk(),
        before,
        "the source-loss refusal changed nothing"
    );
    assert_eq!(
        fs::read(hermes.plugin_root.join(".pohunek-owned.json")).expect("marker"),
        marker_before
    );
    assert_eq!(
        fs::read(hermes.policy_path()).expect("policy"),
        policy_before
    );
    assert_source_hint(&harness, &hermes, "hermes_package_source_missing").await;

    let (code, uninstalled) = harness.json(&hermes.args("uninstall")).await;
    assert_eq!(
        code, 0,
        "marker-backed uninstall survives missing source: {uninstalled:#}"
    );
    assert!(!hermes.plugin_root.exists());
    harness.stop().await;
}

/// An embedded install becomes outdated once a package exists; update switches sources.
#[tokio::test]
async fn update_moves_an_install_to_the_newly_selected_source() {
    let key = test_key();
    let harness = trusted_harness(&key).await;
    let hermes = HermesFixture::new(&harness);

    hermes.install(&harness).await;
    let marker = hermes.marker();
    assert_eq!(marker["source"], "embedded");
    assert!(marker.get("package_digest").is_none());
    assert_eq!(
        fs::read(hermes.plugin_root.join(SKILL_PATH)).expect("installed skill"),
        embedded_operator_file(SKILL_PATH)
    );
    let status = hermes.status(&harness).await;
    assert_eq!(status["outdated"], false, "{status:#}");
    assert_source_findings(&harness, &hermes, false, None, "embedded install").await;

    let (_, first, _, _) = install_official(&harness, &key).await;
    let status = hermes.status(&harness).await;
    assert_eq!(
        status["outdated"], true,
        "a selected package outdates embedded assets"
    );
    assert_source_findings(
        &harness,
        &hermes,
        false,
        None,
        "embedded install, package D1",
    )
    .await;
    let doctor = hermes.doctor(&harness).await;
    assert_eq!(check(&doctor, "asset_current").0, "fail", "{doctor:#}");
    assert!(
        check(&doctor, "asset_current")
            .1
            .contains("selected source"),
        "{doctor:#}"
    );
    hermes.update(&harness).await;
    let marker = hermes.marker();
    assert_eq!(marker["source"], "package");
    assert_eq!(
        marker["package_digest"],
        first.as_str().trim_start_matches("sha256:")
    );
    assert_eq!(
        fs::read(hermes.plugin_root.join(SKILL_PATH)).expect("installed skill"),
        entry(&package_entries(), &format!("operator/{SKILL_PATH}"))
    );
    assert_eq!(hermes.status(&harness).await["outdated"], false);

    let mut second = package_entries();
    set_version(&mut second, "1.0.1");
    let mut skill = entry(&second, &format!("operator/{SKILL_PATH}")).to_vec();
    skill.extend_from_slice(b"\n<!-- second package version -->\n");
    replace_operator_file(&mut second, SKILL_PATH, skill.clone());
    let (bytes, second_digest) = archive_of(&second);
    let archive = write_archive(&harness, "hermes-1.0.1.tar.zst", &bytes);
    let catalog = write_catalog(
        &harness,
        &key,
        2,
        vec![catalog_entry("1.0.1", &second_digest)],
    );
    update_package(&harness, &archive, &catalog, &second_digest).await;
    let status = hermes.status(&harness).await;
    assert_eq!(status["outdated"], true, "a newer selection outdates D1");
    assert_source_findings(&harness, &hermes, false, None, "package D1, D2 selected").await;
    hermes.update(&harness).await;
    let marker = hermes.marker();
    assert_eq!(
        marker["package_digest"],
        second_digest.as_str().trim_start_matches("sha256:")
    );
    assert_eq!(
        marker["operator_sha256"],
        sha256_hex(entry(&second, "operator.toml"))
    );
    assert_eq!(
        fs::read(hermes.plugin_root.join(SKILL_PATH)).expect("installed skill"),
        skill
    );
    assert_eq!(hermes.status(&harness).await["outdated"], false);
    harness.stop().await;
}

/// Signed packages whose sidecar contract is violated never reach the plugin tree.
#[tokio::test]
async fn sidecar_violations_in_a_signed_package_fail_install() {
    let key = test_key();
    let harness = trusted_harness(&key).await;
    let (_, digest, _, _) = install_official(&harness, &key).await;
    let hermes = HermesFixture::new(&harness);
    hermes.install(&harness).await;

    let init_path = "__init__.py";
    let init = String::from_utf8(embedded_operator_file(init_path)).expect("init UTF-8");
    assert_eq!(init.matches(POLICY_TOKEN).count(), 1);
    let mut cases: Vec<(&str, Vec<ArchiveEntry>)> = Vec::new();

    let mut extra = package_entries();
    extra.push(ArchiveEntry {
        path: "operator/x.py".to_owned(),
        contents: b"print('unlisted')\n".to_vec(),
        executable: false,
    });
    cases.push(("an unlisted operator file", extra));

    let mut mismatch = package_entries();
    let tools_digest = sha256_hex(entry(&mismatch, "operator/tools.py"));
    let sidecar = entry_mut(&mut mismatch, "operator.toml");
    let text = String::from_utf8(sidecar.contents.clone()).expect("sidecar UTF-8");
    sidecar.contents = text
        .replacen(&tools_digest, &"0".repeat(64), 1)
        .into_bytes();
    cases.push(("a mismatched sidecar digest", mismatch));

    let mut tokenless = package_entries();
    replace_operator_file(
        &mut tokenless,
        init_path,
        init.replacen(POLICY_TOKEN, "\"/fixed/policy.json\"", 1)
            .into_bytes(),
    );
    cases.push(("an init without the policy token", tokenless));

    let mut repeated = package_entries();
    replace_operator_file(
        &mut repeated,
        init_path,
        format!("{init}\nSECOND_POLICY_PATH = {POLICY_TOKEN}\n").into_bytes(),
    );
    cases.push(("an init with the policy token twice", repeated));

    let mut published = Vec::new();
    for (index, (name, mut entries)) in cases.into_iter().enumerate() {
        let version = format!("1.0.{}", index + 1);
        set_version(&mut entries, &version);
        let (bytes, case_digest) = archive_of(&entries);
        let archive = write_archive(&harness, &format!("hermes-{version}.tar.zst"), &bytes);
        published.push((name, version, case_digest, archive));
    }
    let catalog = write_catalog(
        &harness,
        &key,
        2,
        published
            .iter()
            .map(|(_, version, case_digest, _)| catalog_entry(version, case_digest))
            .collect(),
    );
    for (name, _, case_digest, archive) in &published {
        update_package(&harness, archive, &catalog, case_digest).await;
        assert_source_findings(&harness, &hermes, true, Some("hermes_invalid_asset"), name).await;
        hermes
            .refuses_install_and_update(&harness, "hermes_invalid_asset", name)
            .await;
    }
    assert_source_hint(&harness, &hermes, "hermes_invalid_asset").await;

    select_package(&harness, &digest).await;
    assert_source_findings(&harness, &hermes, false, None, "valid package reselected").await;
    harness.stop().await;
}
