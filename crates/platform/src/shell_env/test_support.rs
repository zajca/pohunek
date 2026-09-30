//! Fixtures shared by the `shell_env` unit tests.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

/// A canonical temporary root with mode 0700.
///
/// macOS temporary directories sit below the `/var` symlink; the canonical
/// path keeps fixtures and assertions on one spelling.
pub(super) struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    /// The canonical root path.
    pub(super) fn path(&self) -> &Path {
        &self.root
    }
}

/// Creates a canonical temporary root.
pub(super) fn fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = fs::canonicalize(temp.path()).expect("canonical temp root");
    Fixture { _temp: temp, root }
}

/// Creates `path` below `root`, every new component with mode 0755 regardless
/// of the process umask.
pub(super) fn make_dir(root: &Path, path: &Path) {
    let relative = path.strip_prefix(root).expect("path below the root");
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        if !current.exists() {
            fs::create_dir(&current).expect("create directory");
            fs::set_permissions(&current, fs::Permissions::from_mode(0o755)).expect("chmod");
        }
    }
}

/// Writes an executable script with mode 0755 and returns its path.
pub(super) fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write script");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod script");
    path
}
