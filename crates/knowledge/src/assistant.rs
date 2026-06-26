//! Assistant knowledge-bundle version, embed, and hash plumbing.

use std::sync::OnceLock;

use include_dir::{include_dir, Dir};
use sha2::{Digest, Sha256};

/// Knowledge bundle version shipped with this binary.
pub const BUNDLE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Whether generated reference concepts were included in the embedded bundle.
///
/// Only consumed by tests; the value is set by the build script.
#[cfg(test)]
const REFERENCE_MODE: &str = env!("POHUNEK_KNOWLEDGE_REFERENCE_MODE");

const EMBEDDED_BUNDLE_CONTENT_HASH: &str = env!("POHUNEK_KNOWLEDGE_CONTENT_HASH");

/// Embedded assistant knowledge bundle.
static BUNDLE: Dir<'static> = include_dir!("$OUT_DIR/knowledge-bundle");

/// Crate-owned handle to the embedded assistant knowledge bundle.
///
/// Wraps the third-party [`include_dir::Dir`] so it never appears in this
/// crate's public contract. Exposes only the read/extract operations consumers
/// need; bundle walking for indexing stays crate-internal.
#[derive(Clone, Copy, Debug)]
pub struct EmbeddedBundle {
    dir: &'static Dir<'static>,
}

impl EmbeddedBundle {
    /// Extract the embedded bundle into `target`, preserving its directory tree.
    ///
    /// # Errors
    ///
    /// Returns an [`std::io::Error`] if any file or directory in the bundle
    /// cannot be created under `target`.
    pub fn extract(self, target: impl AsRef<std::path::Path>) -> std::io::Result<()> {
        self.dir.extract(target)
    }

    /// Return the UTF-8 text of a bundle-relative file, if it exists and is
    /// valid UTF-8.
    #[must_use]
    pub fn get_text(self, path: &str) -> Option<&'static str> {
        self.dir
            .get_file(path)
            .and_then(|file| file.contents_utf8())
    }

    /// Borrow the underlying embedded directory for crate-internal walking.
    pub(crate) fn as_dir(self) -> &'static Dir<'static> {
        self.dir
    }
}

/// Return the memoized content hash for the bundle bytes compiled into this
/// crate.
#[must_use]
pub fn bundle_content_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| EMBEDDED_BUNDLE_CONTENT_HASH.to_owned())
}

/// Return the versioned cache directory name for the embedded bundle.
#[must_use]
pub fn materialized_version_hash() -> String {
    let hash = bundle_content_hash()
        .strip_prefix("sha256:")
        .unwrap_or(bundle_content_hash());
    format!("{BUNDLE_VERSION}-{hash}")
}

/// Return a unique runtime launch id for one assistant materialization.
///
/// # Panics
///
/// Panics if the system clock is set before the Unix epoch.
#[must_use]
pub fn assistant_launch_id(version_hash: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is after unix epoch")
        .as_nanos();
    format!(
        "launch-{version_hash}-{}-{nanos}-{sequence}",
        std::process::id()
    )
}

/// Return the embedded assistant knowledge bundle.
#[must_use]
pub fn embedded_bundle() -> EmbeddedBundle {
    EmbeddedBundle { dir: &BUNDLE }
}

/// Compute a SHA-256 digest for a byte slice.
#[must_use]
pub fn sha256_for_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_version_matches_package_version() {
        assert_eq!(BUNDLE_VERSION, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn bundle_content_hash_is_deterministic_for_same_bytes() {
        let first = sha256_for_bytes(b"same bundle bytes");
        let second = sha256_for_bytes(b"same bundle bytes");

        assert_eq!(first, second);
        assert_eq!(
            first,
            "sha256:06d4332522483bf4e2ed426c2ac497fd4a735fea3246dfae586f679c29a2a16d"
        );
    }

    #[test]
    fn bundle_content_hash_changes_when_bytes_change() {
        let first = sha256_for_bytes(b"bundle A");
        let second = sha256_for_bytes(b"bundle B");

        assert_ne!(first, second);
    }

    #[test]
    fn bundle_content_hash_is_memoized() {
        let first = bundle_content_hash();
        let second = bundle_content_hash();

        assert!(std::ptr::eq(first, second));
        assert!(first.starts_with("sha256:"));
        assert_eq!(first.len(), "sha256:".len() + 64);
    }

    #[test]
    fn materialized_version_hash_combines_version_and_digest_without_prefix() {
        let hash = materialized_version_hash();
        let digest = bundle_content_hash()
            .strip_prefix("sha256:")
            .expect("bundle hash has prefix");

        assert_eq!(hash, format!("{BUNDLE_VERSION}-{digest}"));
    }

    #[test]
    fn assistant_launch_id_includes_version_hash_and_is_unique() {
        let first = assistant_launch_id("0.3.3-abc");
        let second = assistant_launch_id("0.3.3-abc");

        assert!(first.starts_with("launch-0.3.3-abc-"));
        assert!(second.starts_with("launch-0.3.3-abc-"));
        assert_ne!(first, second);
    }

    #[test]
    fn embedded_bundle_contains_manual_index() {
        let index = embedded_bundle()
            .get_text("index.md")
            .expect("embedded bundle contains index.md");

        assert!(index.contains('#'));
        assert_eq!(REFERENCE_MODE, "manual-only");
    }
}
