//! The official Codex, Claude and Hermes runtime packages for daemon
//! integration tests.
//!
//! A host with the official runtime packages installed serves `codex`,
//! `claude` and `hermes` from them. A test that drives one of them builds the archives from
//! `runtime-packages/<runtime>`, the bytes `pohunek plugin install` receives,
//! and installs them through the production `package.install` path with a
//! catalog signed by a throwaway key that only the test's daemon trusts.
//!
//! The installed descriptors keep their version probe, so every fake agent a
//! test launches as `codex`, `claude` or `hermes` answers `--version` with
//! [`supported_banner`] before it does anything else; a fake written in Python
//! runs behind [`python_agent_launcher`].

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]

// Rust guideline compliant 2026-10-12

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ed25519_dalek::SigningKey;
use package::directory::build_directory_archive;
use package::{read_archive, CatalogEntry, Limits, PackageDigest};
use pohunek_daemon::agent::host::definition_from_archive;
use pohunek_daemon::catalog_anchor::CatalogTrust;
use pohunek_daemon::procwatch::HostInspector;
use pohunek_daemon::runtime::{SubprocessWorkerEnvironment, SubprocessWorkerLauncher};
use pohunek_daemon::session::{HostTrustAnchor, SessionRegistry, SessionRegistryConfig};
use pohunek_test_support::catalog::{
    anchor_for, attestations_for, catalog_of, host_platform, root_of, signed, test_key, ANY_CORE,
    RUNTIME_API, WINDOW_END, WINDOW_START,
};
use pohunek_test_support::{worker_binary, workspace_root};
use protocol::{
    BindingProvenance, LaunchBinding, PackageInstallParams, PackageInstallStatus,
    PackageSetEnabledParams, PackageTrust, RuntimeId,
};

/// The runtimes the official packages serve, each built from
/// `runtime-packages/<runtime>` and locked by
/// `compat/<runtime>/compatibility-lock.json`.
const OFFICIAL_RUNTIMES: [&str; 4] = ["codex", "claude", "hermes", "pi"];

/// Sequence number of the one catalog a test signs.
const CATALOG_SEQUENCE: u64 = 1;

/// The flags an install records for a package: whether it is enabled and
/// whether it becomes the selected version of its package.
#[derive(Clone, Copy)]
pub(crate) struct InstallFlags {
    pub(crate) enable: bool,
    pub(crate) select: bool,
}

/// A throwaway catalog signing key and the trust a daemon gives it.
pub(crate) struct OfficialCatalog {
    key: SigningKey,
}

impl OfficialCatalog {
    /// A fresh signing key that exists only in this test process.
    pub(crate) fn new() -> Self {
        Self { key: test_key() }
    }

    /// The catalog trust of a host whose anchor trusts this key.
    pub(crate) fn trust(&self) -> CatalogTrust {
        let anchor = HostTrustAnchor::new(
            vec![root_of(&self.key, WINDOW_START, WINDOW_END)],
            Vec::new(),
        )
        .expect("the throwaway trust anchor");
        CatalogTrust::Loaded(anchor)
    }

    /// `config` with `plugins_dir` as its plugin root and this key as its
    /// catalog trust anchor.
    pub(crate) fn configure(
        &self,
        mut config: SessionRegistryConfig,
        plugins_dir: PathBuf,
    ) -> SessionRegistryConfig {
        config.plugins_dir = Some(plugins_dir);
        config.catalog_trust = self.trust();
        config
    }

    /// Installs the official Codex, Claude and Hermes packages into `registry`
    /// through `package.install`, authorized by one catalog this key signs,
    /// enabled and selected, and returns each runtime's archive digest.
    ///
    /// The archives and the catalog are written below `scratch`. The registry
    /// must hold [`Self::trust`] as its catalog trust.
    pub(crate) async fn install(
        &self,
        registry: &SessionRegistry,
        scratch: &Path,
    ) -> Vec<(&'static str, PackageDigest)> {
        let archives: Vec<_> = OFFICIAL_RUNTIMES
            .into_iter()
            .map(|runtime| (runtime, built_archive(runtime)))
            .collect();
        let entries = archives
            .iter()
            .map(|(runtime, (bytes, digest))| catalog_entry(runtime, bytes, digest))
            .collect();
        let catalog = scratch.join("official-runtime-catalog.json");
        std::fs::write(
            &catalog,
            signed(&self.key, catalog_of(CATALOG_SEQUENCE, WINDOW_END, entries)),
        )
        .expect("write the official runtime catalog");
        let mut installed = Vec::new();
        for (runtime, (bytes, digest)) in archives {
            let archive = scratch.join(format!("{runtime}.tar.zst"));
            std::fs::write(&archive, bytes).expect("write the official package archive");
            let result = registry
                .package_install(PackageInstallParams {
                    archive_path: path_string(&archive),
                    trust: PackageTrust::Catalog {
                        catalog_path: path_string(&catalog),
                    },
                    enable: true,
                    select: true,
                    dry_run: false,
                    accept_commands: false,
                })
                .await
                .unwrap_or_else(|error| {
                    panic!("the official {runtime} package installs: {error:?}")
                });
            assert_eq!(result.status, PackageInstallStatus::Installed, "{runtime}");
            assert_eq!(result.package.digest, digest, "{runtime}");
            installed.push((runtime, digest));
        }
        installed
    }

    /// Writes a release `runtime/` directory below `release` for `runtimes`:
    /// their archives under `packages/` and the catalog listing them, signed by
    /// this key. The archives are the bytes `runtime-packages` builds.
    pub(crate) fn write_release(&self, release: &Path, runtimes: &[&str]) {
        self.write_release_at(release, runtimes, CATALOG_SEQUENCE);
    }

    /// [`Self::write_release`] with the catalog's `sequence`, so a test can
    /// publish a newer or an older release catalog.
    pub(crate) fn write_release_at(&self, release: &Path, runtimes: &[&str], sequence: u64) {
        let built: Vec<(&str, Vec<u8>, PackageDigest)> = runtimes
            .iter()
            .map(|runtime| {
                let (bytes, digest) = built_archive(runtime);
                (*runtime, bytes, digest)
            })
            .collect();
        let archives: Vec<(&str, &[u8], &PackageDigest)> = built
            .iter()
            .map(|(runtime, bytes, digest)| (*runtime, bytes.as_slice(), digest))
            .collect();
        self.write_release_archives(release, sequence, &archives);
    }

    /// Writes a release `runtime/` directory below `release` holding the given
    /// `(runtime, archive bytes, archive digest)` entries, under one catalog at
    /// `sequence`. Each archive is stored as `<runtime>.tar.zst`.
    pub(crate) fn write_release_archives(
        &self,
        release: &Path,
        sequence: u64,
        archives: &[(&str, &[u8], &PackageDigest)],
    ) {
        let packages = release.join("packages");
        std::fs::create_dir_all(&packages).expect("create the release packages directory");
        let entries = archives
            .iter()
            .map(|(runtime, bytes, digest)| {
                std::fs::write(packages.join(format!("{runtime}.tar.zst")), bytes)
                    .expect("write the release package archive");
                catalog_entry(runtime, bytes, digest)
            })
            .collect();
        std::fs::write(
            release.join("runtime-catalog.json"),
            signed(&self.key, catalog_of(sequence, WINDOW_END, entries)),
        )
        .expect("write the release catalog");
    }

    /// The anchor file bytes that trust this key, for the directory of a daemon
    /// executable.
    pub(crate) fn anchor_bytes(&self) -> Vec<u8> {
        anchor_for(&self.key)
    }

    /// Installs one official `archive` of `runtime` into the plugin root
    /// `plugins_dir` through `package.install`, authorized by a catalog at
    /// `sequence` that this key signs, with the given `enable` and `select`
    /// flags, as `pohunek plugin install` records them. Unlike the upgrade
    /// step it installs any version, whatever the registry already holds.
    pub(crate) async fn install_archive(
        &self,
        plugins_dir: &Path,
        scratch: &Path,
        runtime: &str,
        archive: &(Vec<u8>, PackageDigest),
        sequence: u64,
        flags: InstallFlags,
    ) {
        let registry = installer_registry(
            self.configure(installer_config(scratch), plugins_dir.to_path_buf()),
        );
        let (bytes, digest) = archive;
        let catalog = scratch.join(format!("{runtime}-{sequence}-catalog.json"));
        std::fs::write(
            &catalog,
            signed(
                &self.key,
                catalog_of(
                    sequence,
                    WINDOW_END,
                    vec![catalog_entry(runtime, bytes, digest)],
                ),
            ),
        )
        .expect("write the official runtime catalog");
        let file = scratch.join(format!("{runtime}-{sequence}.tar.zst"));
        std::fs::write(&file, bytes).expect("write the official package archive");
        let result = registry
            .package_install(PackageInstallParams {
                archive_path: path_string(&file),
                trust: PackageTrust::Catalog {
                    catalog_path: path_string(&catalog),
                },
                enable: flags.enable,
                select: flags.select,
                dry_run: false,
                accept_commands: false,
            })
            .await
            .unwrap_or_else(|error| panic!("the official {runtime} package installs: {error:?}"));
        assert_eq!(result.status, PackageInstallStatus::Installed, "{runtime}");
        assert_eq!(&result.package.digest, digest, "{runtime}");
    }

    /// Installs the release package of `runtime` from the extracted release
    /// directory `release` into `plugins_dir`, enabled and selected, with the
    /// request `pohunek plugin update` always sends and `pohunek plugin install`
    /// sends when no other version of the package is installed.
    pub(crate) async fn install_release_package(
        &self,
        plugins_dir: &Path,
        scratch: &Path,
        release: &Path,
        runtime: &str,
    ) {
        let registry = installer_registry(
            self.configure(installer_config(scratch), plugins_dir.to_path_buf()),
        );
        let result = registry
            .package_install(PackageInstallParams {
                archive_path: path_string(
                    &release.join("packages").join(format!("{runtime}.tar.zst")),
                ),
                trust: PackageTrust::Catalog {
                    catalog_path: path_string(&release.join("runtime-catalog.json")),
                },
                enable: true,
                select: true,
                dry_run: false,
                accept_commands: false,
            })
            .await
            .unwrap_or_else(|error| panic!("the release {runtime} package installs: {error:?}"));
        assert_eq!(result.status, PackageInstallStatus::Installed, "{runtime}");
    }

    /// Installs the official Codex, Claude and Hermes packages into the plugin root
    /// `plugins_dir` before the registry or daemon binary under test opens it.
    ///
    /// A short-lived production registry, rooted below `scratch` and trusting
    /// this key, performs the catalog-authorized install. A daemon opened over
    /// the plugin root later serves the packages from their recorded official
    /// provenance, with or without this key in its trust anchor.
    pub(crate) async fn preinstall(&self, plugins_dir: &Path, scratch: &Path) {
        let registry = installer_registry(
            self.configure(installer_config(scratch), plugins_dir.to_path_buf()),
        );
        self.install(&registry, scratch).await;
    }
}

/// The registry configuration of an installer run whose workers live below
/// `scratch`.
fn installer_config(scratch: &Path) -> SessionRegistryConfig {
    let worker_home = scratch.join("installer");
    let environment = SubprocessWorkerEnvironment {
        runtime_home: worker_home.join("runtime"),
        state_home: worker_home.join("state"),
        data_home: worker_home.join("data"),
        config_home: worker_home.join("config"),
        cache_home: worker_home.join("cache"),
        home: worker_home.clone(),
        daemon_socket: worker_home.join("daemon.sock"),
    };
    SessionRegistryConfig {
        worker_runtime_root: Some(environment.runtime_home.join("pohunek/workers")),
        worker_state_root: Some(environment.state_home.join("pohunek/workers")),
        supervision: Some(environment.supervision(worker_binary())),
        ..SessionRegistryConfig::default()
    }
}

/// A production registry over `config`, for an installer run.
fn installer_registry(config: SessionRegistryConfig) -> SessionRegistry {
    SessionRegistry::new_production_with_inspector(
        config,
        Arc::new(SubprocessWorkerLauncher::new()),
        Arc::new(HostInspector::new()),
    )
    .expect("the installer registry opens the plugin root")
}

/// Installs a copy of `runtime-packages/<runtime>` whose descriptor declares
/// `package_id` instead of the official id, as a local third-party package
/// with an explicit digest, enabled and selected. Returns its digest.
///
/// The package serves `runtime`'s id without catalog authority, so a daemon
/// over `plugins_dir` judges every official package of that id as a conflict.
pub(crate) async fn preinstall_third_party(
    plugins_dir: &Path,
    scratch: &Path,
    runtime: &str,
    package_id: &str,
) -> PackageDigest {
    let (bytes, digest) = third_party_archive(runtime, package_id);
    let archive = scratch.join(format!("third-party-{runtime}.tar.zst"));
    std::fs::write(&archive, bytes).expect("write the third-party package archive");
    let config = SessionRegistryConfig {
        plugins_dir: Some(plugins_dir.to_path_buf()),
        ..installer_config(scratch)
    };
    let registry = installer_registry(config);
    let result = registry
        .package_install(PackageInstallParams {
            archive_path: path_string(&archive),
            trust: PackageTrust::ExplicitDigest {
                digest: digest.clone(),
            },
            enable: true,
            select: true,
            dry_run: false,
            accept_commands: false,
        })
        .await
        .unwrap_or_else(|error| panic!("the third-party {runtime} package installs: {error:?}"));
    assert_eq!(result.status, PackageInstallStatus::Installed, "{runtime}");
    digest
}

/// Enables the installed package `digest` through the production
/// `package.set_enabled` method, as `pohunek plugin enable --digest` calls it.
pub(crate) async fn enable_package(plugins_dir: &Path, scratch: &Path, digest: &PackageDigest) {
    let config = SessionRegistryConfig {
        plugins_dir: Some(plugins_dir.to_path_buf()),
        ..installer_config(scratch)
    };
    let result = installer_registry(config)
        .package_set_enabled(PackageSetEnabledParams {
            digest: digest.clone(),
            enabled: true,
        })
        .await
        .unwrap_or_else(|error| panic!("the hinted package enables: {error:?}"));
    assert!(result.package.enabled);
}

/// The `--version` banner of the release `compat/<runtime>/compatibility-lock.json`
/// pins, which the version probe of the official package accepts.
pub(crate) fn supported_banner(runtime: &str) -> String {
    let path = workspace_root()
        .join("compat")
        .join(runtime)
        .join("compatibility-lock.json");
    let lock: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
    )
    .expect("the compatibility lock is JSON");
    let release = lock["upstream"]["release"]
        .as_str()
        .expect("the lock pins a release");
    lock["upstream"]["version_output"]
        .as_str()
        .expect("the lock records the version output")
        .replace("{release}", release)
}

/// POSIX shell lines that answer the version probe of the official `runtime`
/// package with [`supported_banner`] and exit, for the top of a fake agent.
pub(crate) fn sh_version_answer(runtime: &str) -> String {
    format!(
        "if [ \"$1\" = --version ]; then printf '%s\\n' '{}'; exit 0; fi\n",
        supported_banner(runtime)
    )
}

/// A POSIX shell launcher for a fake `runtime` agent written in Python: it
/// answers the version probe of the official package with
/// [`supported_banner`] and otherwise runs `script` with its arguments.
///
/// The probe runs under a short budget in an empty environment with a fresh
/// home and temporary directory, where an interpreter start (the macOS
/// `/usr/bin/python3` shim) can be slow or fail; the shell answers within it.
pub(crate) fn python_agent_launcher(runtime: &str, script: &Path) -> String {
    format!(
        "#!/bin/sh\n{}exec /usr/bin/python3 '{}' \"$@\"\n",
        sh_version_answer(runtime),
        script.display()
    )
}

/// The profile keys that pin a host profile over `runtime` to the official
/// package [`OfficialCatalog::install`] installs for it.
pub(crate) fn profile_pin(runtime: &str) -> String {
    let (bytes, digest) = built_archive(runtime);
    let entry = catalog_entry(runtime, &bytes, &digest);
    format!(
        "package = \"{}\"\ndigest = \"{}\"\n",
        entry.package_id.as_str(),
        digest.as_str()
    )
}

/// The launch binding a session launched from the official `runtime` package
/// [`OfficialCatalog::install`] installs records.
pub(crate) fn package_binding(runtime: &str) -> LaunchBinding {
    let (bytes, digest) = built_archive(runtime);
    let archive = read_archive(&bytes, &Limits::DEFAULT).expect("the official archive reads");
    definition_from_archive(archive.entries(), &digest)
        .unwrap_or_else(|error| panic!("the {runtime} package parses: {error:?}"))
        .binding()
        .clone()
}

/// The canonical archive of `runtime-packages/<runtime>` and its digest.
pub(crate) fn built_archive(runtime: &str) -> (Vec<u8>, PackageDigest) {
    archive_of(
        &workspace_root().join("runtime-packages").join(runtime),
        runtime,
    )
}

/// The archive of `runtime` built from a copy of its `runtime-packages`
/// directory whose descriptor declares `version`: the bytes and digest a later
/// release of the same package carries.
pub(crate) fn variant_archive(runtime: &str, version: &str) -> (Vec<u8>, PackageDigest) {
    edited_archive(runtime, &|name, text| {
        if name != "runtime.toml" {
            return text;
        }
        let mut replaced = false;
        text.lines()
            .map(|line| {
                if !replaced && line.starts_with("version = ") {
                    replaced = true;
                    format!("version = \"{version}\"")
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    })
}

/// [`variant_archive`] whose descriptor also names `program` as the agent
/// program the runtime launches.
pub(crate) fn variant_archive_with_program(
    runtime: &str,
    version: &str,
    program: &str,
) -> (Vec<u8>, PackageDigest) {
    edited_archive(runtime, &|name, text| {
        if name != "runtime.toml" {
            return text;
        }
        let mut version_set = false;
        let mut program_set = false;
        text.lines()
            .map(|line| {
                if !version_set && line.starts_with("version = ") {
                    version_set = true;
                    format!("version = \"{version}\"")
                } else if !program_set && line.starts_with("program = ") {
                    program_set = true;
                    format!("program = \"{program}\"")
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    })
}

/// The archive of `runtime-packages/<runtime>` whose descriptor declares
/// `package_id` instead of the official package id.
fn third_party_archive(runtime: &str, package_id: &str) -> (Vec<u8>, PackageDigest) {
    let official = format!("id = \"pohunek.runtime.{runtime}\"");
    edited_archive(runtime, &|name, text| {
        if name == "runtime.toml" {
            text.replacen(&official, &format!("id = \"{package_id}\""), 1)
        } else {
            text
        }
    })
}

/// The archive of a copy of `runtime-packages/<runtime>` whose every file
/// passes through `edit` with its name.
fn edited_archive(
    runtime: &str,
    edit: &dyn Fn(&str, String) -> String,
) -> (Vec<u8>, PackageDigest) {
    let source = workspace_root().join("runtime-packages").join(runtime);
    let scratch = pohunek_test_support::tempdir().expect("a scratch directory");
    copy_edited(&source, scratch.path(), edit);
    archive_of(scratch.path(), runtime)
}

/// Copies the tree `source` into `target`, passing every file's text through
/// `edit` with its file name.
fn copy_edited(source: &Path, target: &Path, edit: &dyn Fn(&str, String) -> String) {
    for entry in std::fs::read_dir(source).expect("the runtime package directory") {
        let path = entry.expect("a package file entry").path();
        let name = path.file_name().expect("a file name").to_owned();
        if path.is_dir() {
            let child = target.join(&name);
            std::fs::create_dir(&child).expect("copy a package directory");
            copy_edited(&path, &child, edit);
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("a package file");
        let edited = edit(&name.to_string_lossy(), text);
        std::fs::write(target.join(name), edited).expect("copy a package file");
    }
}

fn archive_of(directory: &Path, runtime: &str) -> (Vec<u8>, PackageDigest) {
    let bytes = build_directory_archive(directory, &Limits::DEFAULT)
        .unwrap_or_else(|error| panic!("the {runtime} package builds: {error:?}"));
    let digest = read_archive(&bytes, &Limits::DEFAULT)
        .expect("the official archive reads")
        .digest()
        .clone();
    (bytes, digest)
}

/// The catalog entry authorizing the archive `bytes` of `runtime` for the
/// host platform, with the package identity its descriptor declares.
fn catalog_entry(runtime: &str, bytes: &[u8], digest: &PackageDigest) -> CatalogEntry {
    let archive = read_archive(bytes, &Limits::DEFAULT).expect("the official archive reads");
    let definition = definition_from_archive(archive.entries(), digest)
        .unwrap_or_else(|error| panic!("the {runtime} package parses: {error:?}"));
    let BindingProvenance::Package { package, .. } = definition.binding().provenance.clone() else {
        panic!("the {runtime} package has package provenance");
    };
    CatalogEntry {
        package_id: package.id,
        runtime_id: RuntimeId::parse(runtime).expect("runtime id"),
        version: package.version,
        digest: digest.clone(),
        runtime_api: RUNTIME_API,
        platforms: vec![host_platform()],
        attestations: attestations_for(&[host_platform()]),
        core: ANY_CORE.to_owned(),
    }
}

fn path_string(path: &Path) -> String {
    path.to_str().expect("UTF-8 fixture path").to_owned()
}
