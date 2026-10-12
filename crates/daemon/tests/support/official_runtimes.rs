//! The official Codex and Claude runtime packages for daemon integration tests.
//!
//! A host with the official runtime packages installed serves `codex` and
//! `claude` from them. A test that drives one of them builds the archives from
//! `runtime-packages/<runtime>`, the bytes `pohunek plugin install` receives,
//! and installs them through the production `package.install` path with a
//! catalog signed by a throwaway key that only the test's daemon trusts.
//!
//! The installed descriptors keep their version probe, so every fake agent a
//! test launches as `codex` or `claude` answers `--version` with
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
    attestations_for, catalog_of, host_platform, root_of, signed, test_key, ANY_CORE, RUNTIME_API,
    WINDOW_END, WINDOW_START,
};
use pohunek_test_support::{worker_binary, workspace_root};
use protocol::{
    BindingProvenance, LaunchBinding, PackageInstallParams, PackageInstallStatus, PackageTrust,
    RuntimeId,
};

/// The runtimes the official packages serve, each built from
/// `runtime-packages/<runtime>` and locked by
/// `compat/<runtime>/compatibility-lock.json`.
const OFFICIAL_RUNTIMES: [&str; 2] = ["codex", "claude"];

/// Sequence number of the one catalog a test signs.
const CATALOG_SEQUENCE: u64 = 1;

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

    /// Installs the official Codex and Claude packages into `registry`
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

    /// Installs the official Codex and Claude packages into the plugin root
    /// `plugins_dir` before the registry or daemon binary under test opens it.
    ///
    /// A short-lived production registry, rooted below `scratch` and trusting
    /// this key, performs the catalog-authorized install. A daemon opened over
    /// the plugin root later serves the packages from their recorded official
    /// provenance, with or without this key in its trust anchor.
    pub(crate) async fn preinstall(&self, plugins_dir: &Path, scratch: &Path) {
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
        let config = SessionRegistryConfig {
            worker_runtime_root: Some(environment.runtime_home.join("pohunek/workers")),
            worker_state_root: Some(environment.state_home.join("pohunek/workers")),
            supervision: Some(environment.supervision(worker_binary())),
            ..SessionRegistryConfig::default()
        };
        let registry = SessionRegistry::new_production_with_inspector(
            self.configure(config, plugins_dir.to_path_buf()),
            Arc::new(SubprocessWorkerLauncher::new()),
            Arc::new(HostInspector::new()),
        )
        .expect("the installer registry opens the plugin root");
        self.install(&registry, scratch).await;
    }
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
fn built_archive(runtime: &str) -> (Vec<u8>, PackageDigest) {
    let directory = workspace_root().join("runtime-packages").join(runtime);
    let bytes = build_directory_archive(&directory, &Limits::DEFAULT)
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
