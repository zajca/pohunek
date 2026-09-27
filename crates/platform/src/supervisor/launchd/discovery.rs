//! Classifies the entries of the private worker definitions directory.
//!
//! Only a [`TrustedDir`], the plist parser, and the namespace grammar are
//! involved, so the classification is target-neutral and unit-tested on every
//! host; inspecting the candidates with `launchctl` stays in the macOS backend.

use std::ffi::OsString;

use super::super::{Error, Namespace, WorkerKey};
use super::plist::{self, MAX_DEFINITION_BYTES};
use super::registration::{fs_error, DEFINITION_MODE, DEFINITION_SUFFIX};
use crate::filesystem::{FsError, TrustedDir};

// Rust guideline compliant 2026-09-27

/// Definition files of one namespace, split by whether discovery may inspect
/// them.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Entries {
    /// Workers whose definition parses and whose `Label` names its file.
    pub(super) candidates: Vec<WorkerKey>,
    /// File names of this namespace that are not a definition this backend
    /// wrote: unparsable, labeled differently from the file name, or not a
    /// private regular file. They are never loaded, inspected, or removed.
    pub(super) rejected: Vec<String>,
}

/// Classifies the enumerated `names` of the definitions `directory`.
///
/// Names that are not UTF-8, lack the definition suffix, or do not parse as a
/// worker label of `namespace` belong to someone else and are skipped
/// silently. A file that vanished since enumeration is skipped as retired.
///
/// # Errors
///
/// Returns [`Error::Operation`] when reading a definition fails for a reason
/// other than the file being absent or unsafe.
pub(super) fn classify(
    directory: &TrustedDir,
    names: Vec<OsString>,
    namespace: &Namespace,
    operation: &'static str,
) -> Result<Entries, Error> {
    let mut entries = Entries::default();
    for name in names {
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(label) = name.strip_suffix(DEFINITION_SUFFIX) else {
            continue;
        };
        let Ok(key) = namespace.parse_worker_label(label) else {
            continue;
        };
        match directory.read_file(name, DEFINITION_MODE, MAX_DEFINITION_BYTES) {
            Ok(bytes) => match plist::parse(&bytes) {
                Ok(stored) if stored.label == label => entries.candidates.push(key),
                Ok(_) | Err(_) => entries.rejected.push(name.to_owned()),
            },
            // Retired between enumeration and read.
            Err(error) if error.io_kind() == Some(std::io::ErrorKind::NotFound) => {}
            // Not a file this backend wrote.
            Err(
                FsError::UnsafeType { .. }
                | FsError::UnsafeOwner { .. }
                | FsError::UnsafeMode { .. }
                | FsError::UnsafeAcl { .. }
                | FsError::UnsafeLinkCount { .. }
                | FsError::FileTooLarge { .. },
            ) => entries.rejected.push(name.to_owned()),
            Err(error) => return Err(fs_error(operation)(error)),
        }
    }
    Ok(entries)
}

/// Refuses a destructive discovery that rejected an entry of this namespace.
///
/// A rejected definition may belong to a job that is still loaded (a corrupted
/// or tampered file of a live worker), and its process is never matched, so a
/// caller that deletes versions or retires around the result could remove the
/// executable that job runs from.
///
/// # Errors
///
/// Returns [`Error::InvalidData`] naming the first rejected entry and the
/// count when `rejected` is not empty.
pub(super) fn refuse_rejected(operation: &'static str, rejected: &[String]) -> Result<(), Error> {
    match rejected.first() {
        None => Ok(()),
        Some(first) => Err(Error::InvalidData {
            operation,
            detail: format!(
                "{} worker definition(s) of this namespace cannot be verified, first `{first}`; \
                 a job loaded from one may still run",
                rejected.len()
            ),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::PathBuf;

    use super::*;

    /// Namespace every fixture definition belongs to.
    const NAMESPACE: &str = "0123456789ab";

    struct Fixture {
        _root: tempfile::TempDir,
        path: PathBuf,
        directory: TrustedDir,
        namespace: Namespace,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("temporary directory");
            let base = std::fs::canonicalize(root.path()).expect("canonical temporary directory");
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))
                .expect("private mode");
            let path = base.join("definitions");
            let directory =
                TrustedDir::open_or_create_absolute(&path, 0o700).expect("private directory");
            Self {
                _root: root,
                path,
                directory,
                namespace: Namespace::parse(NAMESPACE).expect("valid namespace"),
            }
        }

        fn label(&self, session: &str) -> String {
            self.namespace
                .worker_label(&WorkerKey::new(session, "abcd2345").expect("valid worker key"))
        }

        fn plant(&self, file_name: &str, contents: &[u8], mode: u32) {
            let path = self.path.join(file_name);
            std::fs::write(&path, contents).expect("definition written");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                .expect("definition mode");
        }

        fn classify(&self) -> Entries {
            let names = self.directory.entry_names().expect("directory enumerates");
            let mut entries = classify(&self.directory, names, &self.namespace, "discover")
                .expect("classification succeeds");
            entries.candidates.sort();
            entries.rejected.sort();
            entries
        }
    }

    fn document(label: &str) -> Vec<u8> {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\
             <key>Label</key><string>{label}</string>\
             <key>ProgramArguments</key><array><string>/bin/sh</string></array>\
             <key>ExitTimeOut</key><integer>5</integer></dict></plist>"
        )
        .into_bytes()
    }

    #[test]
    fn own_definitions_this_backend_did_not_write_are_rejected() {
        let fixture = Fixture::new();
        let valid = fixture.label("s-1");
        fixture.plant(&format!("{valid}.plist"), &document(&valid), 0o600);
        let malformed = fixture.label("s-2");
        fixture.plant(&format!("{malformed}.plist"), b"not a plist", 0o600);
        let mislabeled = fixture.label("s-3");
        fixture.plant(
            &format!("{mislabeled}.plist"),
            &document(&fixture.label("s-4")),
            0o600,
        );
        let unsafe_mode = fixture.label("s-5");
        fixture.plant(
            &format!("{unsafe_mode}.plist"),
            &document(&unsafe_mode),
            0o644,
        );
        let symlinked = fixture.label("s-6");
        std::os::unix::fs::symlink(
            fixture.path.join(format!("{valid}.plist")),
            fixture.path.join(format!("{symlinked}.plist")),
        )
        .expect("symlink planted");

        let entries = fixture.classify();

        assert_eq!(
            entries.candidates,
            vec![WorkerKey::new("s-1", "abcd2345").expect("valid worker key")]
        );
        let mut expected: Vec<String> = [malformed, mislabeled, unsafe_mode, symlinked]
            .iter()
            .map(|label| format!("{label}.plist"))
            .collect();
        expected.sort();
        assert_eq!(entries.rejected, expected);
        let error = refuse_rejected("discover", &entries.rejected)
            .expect_err("a strict discovery refuses rejected own definitions");
        assert!(
            matches!(
                &error,
                Error::InvalidData { operation: "discover", detail }
                    if detail.starts_with("4 worker definition(s)")
                        && detail.contains(&expected[0])
            ),
            "{error}"
        );
    }

    #[test]
    fn foreign_names_are_skipped_and_never_refuse_a_strict_discovery() {
        let fixture = Fixture::new();
        let other = Namespace::parse("ba9876543210").expect("valid namespace");
        let foreign =
            other.worker_label(&WorkerKey::new("s-1", "abcd2345").expect("valid worker key"));
        fixture.plant(&format!("{foreign}.plist"), b"not a plist", 0o644);
        fixture.plant(
            &format!("{}.plist", fixture.namespace.daemon_label()),
            b"not a plist",
            0o600,
        );
        fixture.plant("unrelated.plist", b"not a plist", 0o600);
        fixture.plant(&fixture.label("s-2"), b"no suffix", 0o600);
        fixture.plant(".pohunek-launchd-removed-x", b"quarantined", 0o600);

        let entries = fixture.classify();

        assert_eq!(entries, Entries::default());
        refuse_rejected("discover", &entries.rejected)
            .expect("foreign entries never refuse a strict discovery");
    }

    #[test]
    fn a_vanished_definition_is_skipped() {
        let fixture = Fixture::new();
        let label = fixture.label("s-1");
        let names = vec![OsString::from(format!("{label}.plist"))];

        let entries = classify(&fixture.directory, names, &fixture.namespace, "discover")
            .expect("a vanished definition is a retirement race");

        assert_eq!(entries, Entries::default());
    }
}
