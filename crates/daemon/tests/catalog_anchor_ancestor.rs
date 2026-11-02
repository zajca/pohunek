//! Scenarios of the directory-chain policy of the catalog trust anchor.
//!
//! `load_from_directory` runs against a real directory chain whose group and
//! mode vary. The macOS rows need a runner in the `admin` (gid 80) and `staff`
//! (gid 20) groups and fail, never skip, when it is not; they cover the stock
//! Homebrew shape (`user:admin 775`). The rows without a platform gate run on
//! every host.

// Rust guideline compliant 2026-11-02

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use package::ANCHOR_FILE_NAME;
use pohunek_daemon::catalog_anchor::{load_from_directory, AnchorFault, CatalogTrust};
use pohunek_test_support::catalog::{anchor_for, test_key};
use pohunek_test_support::tempdir;

/// A directory that holds the daemon executable and its anchor.
struct Chain {
    _root: tempfile::TempDir,
    directory: PathBuf,
}

impl Chain {
    /// A private root with the directory `bin` below it at `mode`, holding a
    /// valid anchor file (mode 0644).
    fn new(mode: u32) -> Self {
        let root = tempdir().expect("create a private root");
        let directory = root.path().join("bin");
        fs::create_dir(&directory).expect("create the executable directory");
        fs::set_permissions(&directory, fs::Permissions::from_mode(mode))
            .expect("set the directory mode");
        let chain = Self {
            _root: root,
            directory,
        };
        fs::write(chain.anchor(), anchor_for(&test_key())).expect("write the anchor");
        fs::set_permissions(chain.anchor(), fs::Permissions::from_mode(0o644))
            .expect("set the anchor mode");
        chain
    }

    fn anchor(&self) -> PathBuf {
        self.directory.join(ANCHOR_FILE_NAME)
    }

    fn load(&self) -> CatalogTrust {
        load_from_directory(&self.directory)
    }
}

fn assert_loaded(trust: &CatalogTrust) {
    assert!(matches!(trust, CatalogTrust::Loaded(_)), "{trust:?}");
}

#[test]
fn a_private_directory_chain_loads_the_anchor() {
    assert_loaded(&Chain::new(0o755).load());
}

#[test]
fn a_group_writable_directory_outside_the_admin_group_is_refused() {
    assert_eq!(
        Chain::new(0o775).load(),
        CatalogTrust::Invalid(AnchorFault::UnsafeDirectory)
    );
}

#[cfg(target_os = "macos")]
mod admin_chain {
    use std::path::Path;

    use super::*;

    /// The macOS `admin` group.
    const ADMIN_GID: u32 = 80;

    /// The macOS `staff` group, a group other than `admin`.
    const STAFF_GID: u32 = 20;

    /// Fails hard unless the runner belongs to `admin` and `staff`.
    fn assert_groups() {
        let output = std::process::Command::new("/usr/bin/id")
            .arg("-G")
            .output()
            .expect("run id -G");
        let groups: Vec<u32> = String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .filter_map(|group| group.parse().ok())
            .collect();
        for required in [ADMIN_GID, STAFF_GID] {
            assert!(
                groups.contains(&required),
                "the runner must be in group {required}, it is in {groups:?}"
            );
        }
    }

    fn chain(gid: u32, mode: u32) -> Chain {
        assert_groups();
        let chain = Chain::new(mode);
        regroup(&chain.directory, gid);
        chain
    }

    fn regroup(path: &Path, gid: u32) {
        std::os::unix::fs::chown(path, None, Some(gid)).expect("set the group");
    }

    #[test]
    fn a_user_owned_admin_group_writable_directory_loads_the_anchor() {
        assert_loaded(&chain(ADMIN_GID, 0o775).load());
    }

    #[test]
    fn a_group_writable_directory_of_another_group_is_refused() {
        assert_eq!(
            chain(STAFF_GID, 0o775).load(),
            CatalogTrust::Invalid(AnchorFault::UnsafeDirectory)
        );
    }

    #[test]
    fn a_world_writable_admin_directory_is_refused() {
        assert_eq!(
            chain(ADMIN_GID, 0o777).load(),
            CatalogTrust::Invalid(AnchorFault::UnsafeDirectory)
        );
    }

    #[test]
    fn a_group_writable_anchor_in_an_admin_directory_is_refused() {
        let chain = chain(ADMIN_GID, 0o775);
        fs::set_permissions(chain.anchor(), fs::Permissions::from_mode(0o664))
            .expect("make the anchor group-writable");
        assert_eq!(chain.load(), CatalogTrust::Invalid(AnchorFault::UnsafeFile));
    }

    #[test]
    fn a_hard_linked_anchor_in_an_admin_directory_is_refused() {
        let chain = chain(ADMIN_GID, 0o775);
        fs::hard_link(chain.anchor(), chain.directory.join("second-name"))
            .expect("hard-link the anchor");
        assert_eq!(chain.load(), CatalogTrust::Invalid(AnchorFault::UnsafeFile));
    }
}
