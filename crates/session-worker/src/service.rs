//! Verifies the service configuration file a supervised worker starts with.
//!
//! A worker started by systemd or launchd receives `--service-config <path>`
//! naming the installation's `service.toml`. The worker refuses to start when
//! that file is not a trustworthy owner-private regular file, so a broken
//! installation fails before the worker binds its socket. Typed parsing of the
//! file's contents belongs to the `pohunek-service-config` crate.

// Rust guideline compliant 2026-09-24

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Permission bit that lets the owner read a file.
const OWNER_READ: u32 = 0o400;
/// Permission bits that let anyone but the owner modify a file.
const NON_OWNER_WRITE: u32 = 0o022;
/// Permission bits carried by `st_mode`, excluding the file type.
const PERMISSION_BITS: u32 = 0o7777;

/// Reports why a service configuration file cannot be trusted.
#[derive(Debug, thiserror::Error)]
pub enum ServiceConfigError {
    /// The path is relative.
    #[error("service configuration path {} must be absolute", path.display())]
    NotAbsolute {
        /// Rejected path.
        path: PathBuf,
    },
    /// The path could not be inspected, including when it does not exist.
    #[error("service configuration {} is not accessible: {source}", path.display())]
    Inaccessible {
        /// Inspected path.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// The path names a symlink, directory, or other non-regular file.
    #[error("service configuration {} is not a regular file", path.display())]
    NotRegularFile {
        /// Inspected path.
        path: PathBuf,
    },
    /// Another user owns the file.
    #[error(
        "service configuration {} is owned by uid {owner}, expected {expected}",
        path.display()
    )]
    ForeignOwner {
        /// Inspected path.
        path: PathBuf,
        /// Observed owner.
        owner: u32,
        /// Effective user of this worker.
        expected: u32,
    },
    /// The owner cannot read the file, or other users can modify it.
    #[error(
        "service configuration {} has mode {mode:04o}; it must be owner-readable \
         and not group- or world-writable",
        path.display()
    )]
    UnsafeMode {
        /// Inspected path.
        path: PathBuf,
        /// Observed permission bits.
        mode: u32,
    },
}

/// Verifies that `path` is an owner-private service configuration file.
///
/// The file must be an absolute, regular, non-symlink file owned by the
/// worker's effective user, readable by that owner, and not writable by group
/// or others.
///
/// # Errors
///
/// Returns [`ServiceConfigError`] naming the first violated requirement.
pub fn load_service_config(path: &Path) -> Result<(), ServiceConfigError> {
    if !path.is_absolute() {
        return Err(ServiceConfigError::NotAbsolute {
            path: path.to_path_buf(),
        });
    }
    let metadata =
        std::fs::symlink_metadata(path).map_err(|source| ServiceConfigError::Inaccessible {
            path: path.to_path_buf(),
            source,
        })?;
    if !metadata.file_type().is_file() {
        return Err(ServiceConfigError::NotRegularFile {
            path: path.to_path_buf(),
        });
    }
    let expected = rustix::process::geteuid().as_raw();
    if metadata.uid() != expected {
        return Err(ServiceConfigError::ForeignOwner {
            path: path.to_path_buf(),
            owner: metadata.uid(),
            expected,
        });
    }
    let mode = metadata.mode() & PERMISSION_BITS;
    if mode & OWNER_READ == 0 || mode & NON_OWNER_WRITE != 0 {
        return Err(ServiceConfigError::UnsafeMode {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};

    use super::*;

    fn config_file(mode: u32) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().expect("create temporary directory");
        let path = root.path().join("service.toml");
        fs::write(&path, b"schema_version = 1\n").expect("write service configuration");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("chmod");
        (root, path)
    }

    #[test]
    fn an_owner_private_regular_file_is_accepted() {
        for mode in [0o600, 0o400, 0o644] {
            let (_root, path) = config_file(mode);
            load_service_config(&path).expect("owner-private configuration");
        }
    }

    #[test]
    fn a_relative_path_is_rejected() {
        assert!(matches!(
            load_service_config(Path::new("service.toml")),
            Err(ServiceConfigError::NotAbsolute { .. })
        ));
    }

    #[test]
    fn a_missing_file_is_rejected() {
        let root = tempfile::tempdir().expect("create temporary directory");

        let error = load_service_config(&root.path().join("service.toml"))
            .expect_err("missing file must fail");

        assert!(matches!(
            error,
            ServiceConfigError::Inaccessible { ref source, .. }
                if source.kind() == std::io::ErrorKind::NotFound
        ));
    }

    #[test]
    fn directories_and_symlinks_are_rejected() {
        let (root, path) = config_file(0o600);
        let link = root.path().join("link.toml");
        symlink(&path, &link).expect("create symlink");

        assert!(matches!(
            load_service_config(root.path()),
            Err(ServiceConfigError::NotRegularFile { .. })
        ));
        assert!(matches!(
            load_service_config(&link),
            Err(ServiceConfigError::NotRegularFile { .. })
        ));
    }

    #[test]
    fn unreadable_or_shared_writable_files_are_rejected() {
        for mode in [0o200, 0o000, 0o620, 0o602, 0o666] {
            let (_root, path) = config_file(mode);
            assert!(
                matches!(
                    load_service_config(&path),
                    Err(ServiceConfigError::UnsafeMode { mode: observed, .. }) if observed == mode
                ),
                "mode {mode:04o} must be rejected"
            );
        }
    }
}
