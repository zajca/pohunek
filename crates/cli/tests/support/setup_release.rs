//! A release directory for bare `pohunek setup`: signed like a release, with
//! a throwaway trust anchor beside the daemon and a `runtime/` of real official
//! packages under a catalog that key signs, and sealed like an extracted daemon
//! archive, with the archive's own `packaging/verify-archive` beside its files
//! and a `MANIFEST` that `packaging/write-manifest` writes over all of them.

// Rust guideline compliant 2026-10-15

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use ed25519_dalek::SigningKey;
use package::directory::build_directory_archive;
use package::{read_archive, CatalogEntry, Limits, PackageDigest, ANCHOR_FILE_NAME};
use pohunek_daemon::agent::host::definition_from_archive;
use pohunek_test_support::catalog::{
    anchor_for, attestations_for, catalog_of, host_platform, signed, ANY_CORE, RUNTIME_API,
    WINDOW_END,
};
use protocol::{BindingProvenance, RuntimeId};

/// Runtime packages the release carries.
pub(crate) const RELEASE_RUNTIMES: [&str; 3] = ["claude", "codex", "pi"];

/// Sequence number of the release catalog.
const CATALOG_SEQUENCE: u64 = 1;

/// The release's `runtime/` entry beside the daemon, its package directory and
/// archive suffix, and its signed catalog, as the release daemon reads them.
const RUNTIME_DIR: &str = "runtime";
const PACKAGES_DIR: &str = "packages";
const ARCHIVE_SUFFIX: &str = ".tar.zst";
const CATALOG_FILE: &str = "runtime-catalog.json";

/// Version written into the manifest.
const SEALED_VERSION: &str = "0.0.1";

/// The target triple and signing state of a daemon archive built for this
/// host, and the macOS version it demands.
#[cfg(target_os = "macos")]
const SEALED_TARGET: (&str, &str, Option<&str>) = ("aarch64-apple-darwin", "adhoc", Some("14.0"));
#[cfg(not(target_os = "macos"))]
const SEALED_TARGET: (&str, &str, Option<&str>) = ("x86_64-unknown-linux-gnu", "none", None);

/// Search path of the sealing script: the system tools only.
const SYSTEM_PATH: &str = "/usr/bin:/bin";

/// Copies the archive verifier into `release` and seals every file of it,
/// verifier included, into its `MANIFEST`. `release` must already hold
/// `pohunek`, `pohunekd` and `pohunek-sessiond`.
pub(crate) fn seal(release: &Path) {
    let root = pohunek_test_support::workspace_root();
    let packaging = release.join("packaging");
    fs::create_dir_all(&packaging).expect("create the packaging directory");
    fs::copy(
        root.join("packaging/verify-archive"),
        packaging.join("verify-archive"),
    )
    .expect("copy the archive verifier");
    let (target, signing, minimum) = SEALED_TARGET;
    let output = Command::new("/bin/sh")
        .arg(root.join("packaging/write-manifest"))
        .arg(release)
        .args(["daemon", SEALED_VERSION, target, signing])
        .args(minimum)
        .env_clear()
        .env("PATH", SYSTEM_PATH)
        .output()
        .expect("run write-manifest");
    assert!(
        output.status.success(),
        "write-manifest seals the release: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Writes the trust anchor of `key` beside the daemon in `release`, and the
/// release's `runtime/`: one archive per release runtime under `packages/`,
/// built from `runtime-packages`, and a catalog of them signed by `key`.
///
/// A release that carries only the packages of `runtimes` has a catalog that
/// lists no package of any other runtime for this platform.
pub(crate) fn sign_runtimes(release: &Path, key: &SigningKey, runtimes: &[&str]) {
    fs::write(release.join(ANCHOR_FILE_NAME), anchor_for(key)).expect("write the trust anchor");
    let runtime = release.join(RUNTIME_DIR);
    let packages = runtime.join(PACKAGES_DIR);
    fs::create_dir_all(&packages).expect("packages directory");
    let mut entries = Vec::new();
    for name in runtimes {
        let (bytes, entry) = official_archive(name);
        fs::write(packages.join(format!("{name}{ARCHIVE_SUFFIX}")), bytes)
            .expect("write the package archive");
        entries.push(entry);
    }
    let catalog = catalog_of(CATALOG_SEQUENCE, WINDOW_END, entries);
    fs::write(runtime.join(CATALOG_FILE), signed(key, catalog)).expect("write the catalog");
}

/// The package archive of `runtime-packages/<runtime>`, built the way
/// `pohunek plugin install` receives it, and the catalog entry that
/// authorizes it on this host.
fn official_archive(runtime: &str) -> (Vec<u8>, CatalogEntry) {
    let source = pohunek_test_support::workspace_root()
        .join("runtime-packages")
        .join(runtime);
    let bytes = build_directory_archive(&source, &Limits::DEFAULT)
        .unwrap_or_else(|error| panic!("the {runtime} package builds: {error:?}"));
    let archive = read_archive(&bytes, &Limits::DEFAULT).expect("the archive reads");
    let digest: PackageDigest = archive.digest().clone();
    let definition = definition_from_archive(archive.entries(), &digest)
        .unwrap_or_else(|error| panic!("the {runtime} package parses: {error:?}"));
    let BindingProvenance::Package { package, .. } = definition.binding().provenance.clone() else {
        panic!("the {runtime} package has package provenance");
    };
    let entry = CatalogEntry {
        package_id: package.id,
        runtime_id: RuntimeId::parse(runtime).expect("runtime id"),
        version: package.version,
        digest,
        runtime_api: RUNTIME_API,
        platforms: vec![host_platform()],
        attestations: attestations_for(&[host_platform()]),
        core: ANY_CORE.to_owned(),
    };
    (bytes, entry)
}

/// Copies this build's `pohunek`, `pohunekd` and `pohunek-sessiond` into
/// `release`. The daemon and the worker are `POHUNEK_DAEMON_BIN` and
/// `POHUNEK_WORKER_BIN` when set, else the binaries beside the CLI.
pub(crate) fn stage_binaries(release: &Path) {
    fs::create_dir_all(release).expect("create the release directory");
    let cli = pohunek_test_support::bin_exe("pohunek");
    for (name, variable) in [
        ("pohunekd", "POHUNEK_DAEMON_BIN"),
        ("pohunek-sessiond", "POHUNEK_WORKER_BIN"),
    ] {
        let source =
            std::env::var_os(variable).map_or_else(|| cli.with_file_name(name), PathBuf::from);
        assert!(
            source.is_file(),
            "{} is missing; build `cargo build -p pohunek-daemon -p pohunek-session-worker --bins` or set {variable}",
            source.display()
        );
        fs::copy(&source, release.join(name)).expect("copy a release binary");
    }
    fs::copy(&cli, release.join("pohunek")).expect("copy the CLI");
}
