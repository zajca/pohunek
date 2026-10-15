//! A release directory sealed as an extracted daemon archive, the way the
//! release workflow seals one: the archive's own `packaging/verify-archive`
//! beside its files and a `MANIFEST` that `packaging/write-manifest` writes
//! over all of them.

// Rust guideline compliant 2026-10-15

use std::fs;
use std::path::Path;
use std::process::Command;

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
