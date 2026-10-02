//! Classifies the entries of the private worker definitions directory.
//!
//! Only a [`TrustedDir`], the plist parser, the namespace grammar, and a
//! [`Launchctl`] runner are involved, so the classification is target-neutral
//! and unit-tested on every host; inspecting the candidates stays in the macOS
//! backend.
//!
//! The directory is the registry of this namespace's workers: launchd offers
//! no label enumeration without parsing `launchctl` output, and every worker
//! is bootstrapped from a `<label>.plist` that `start` publishes here first,
//! in a directory it creates. A worker definition is removed only once its
//! label is absent, and the directory itself only by an uninstall that has
//! retired every worker. A loaded worker therefore always has its
//! `<label>.plist` here, or, while a registration is interrupted, only its
//! set-aside `.<label>.plist.replaced`, which [`set_aside_loaded`] probes. A
//! missing directory is an empty registry.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};

use super::super::{Error, Namespace, WorkerKey};
use super::launchctl::Launchctl;
use super::plist::{self, MAX_DEFINITION_BYTES};
use super::registration::{
    definition_name, fs_error, probe, set_aside_label, DEFINITION_MODE, DEFINITION_SUFFIX,
};
use super::Presence;
use crate::filesystem::{FsError, TrustedDir};

// Rust guideline compliant 2026-09-28

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
    /// Workers whose only definition is the set-aside file an interrupted
    /// registration left: `.<label>.plist.replaced` is listed and
    /// `<label>.plist` is not.
    pub(super) set_aside: Vec<WorkerKey>,
}

/// Classifies the enumerated `names` of the definitions `directory`.
///
/// Names that are not UTF-8, lack the definition suffix, or do not parse as a
/// worker label of `namespace` belong to someone else and are skipped
/// silently. A file that vanished since enumeration is skipped as retired.
/// A set-aside name of a worker of `namespace` is listed in
/// [`Entries::set_aside`] when `names` lacks that worker's `<label>.plist`;
/// otherwise the definition stands for the job and the set-aside file is
/// obsolete. Its content is never read: only the label's presence matters.
///
/// # Errors
///
/// Returns [`Error::Operation`] when reading a definition fails for a reason
/// other than the file being absent or unsafe.
pub(super) fn classify(
    directory: &TrustedDir,
    names: &[OsString],
    namespace: &Namespace,
    operation: &'static str,
) -> Result<Entries, Error> {
    let mut entries = Entries::default();
    let listed: HashSet<&OsStr> = names.iter().map(OsString::as_os_str).collect();
    for name in names {
        let Some(name) = name.to_str() else {
            continue;
        };
        if let Some(label) = set_aside_label(name) {
            if let Ok(key) = namespace.parse_worker_label(label) {
                if !listed.contains(OsStr::new(&definition_name(label))) {
                    entries.set_aside.push(key);
                }
            }
            continue;
        }
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

/// Returns whether any worker in `set_aside` is loaded in `domain`.
///
/// The set-aside file is not where the job's definition is read from, so a
/// loaded label among them has no inspectable definition: the discovery that
/// sees one is incomplete until its registration settles. An absent label runs
/// nothing, and [`super::registration::recover`] settles its file at the
/// label's next registration or retirement. Nothing is moved here, so a
/// discovery never interferes with a registration in progress.
///
/// # Errors
///
/// Returns the first `print` failure other than an absent label.
pub(super) async fn set_aside_loaded(
    launchctl: &Launchctl,
    domain: &str,
    namespace: &Namespace,
    set_aside: &[WorkerKey],
    operation: &'static str,
) -> Result<bool, Error> {
    let mut loaded = false;
    for key in set_aside {
        let label = namespace.worker_label(key);
        if probe(launchctl, domain, &label, operation).await? == Presence::Loaded {
            tracing::event!(
                name: "supervisor.discover.set_aside_loaded",
                tracing::Level::WARN,
                supervisor.label = label.as_str(),
                "loaded worker {{supervisor.label}} has only a set-aside definition"
            );
            loaded = true;
        }
    }
    Ok(loaded)
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
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::super::registration::set_aside_name;
    use super::*;

    const DOMAIN: &str = "gui/501";

    /// Deadline of one fake `launchctl` command; long enough for `sh` to
    /// start on a loaded runner.
    const DEADLINE: Duration = Duration::from_secs(2);

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
            let root = pohunek_test_support::tempdir().expect("temporary directory");
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
            let mut entries = classify(&self.directory, &names, &self.namespace, "discover")
                .expect("classification succeeds");
            entries.candidates.sort();
            entries.rejected.sort();
            entries.set_aside.sort();
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

        let entries = classify(&fixture.directory, &names, &fixture.namespace, "discover")
            .expect("a vanished definition is a retirement race");

        assert_eq!(entries, Entries::default());
    }

    fn key(session: &str) -> WorkerKey {
        WorkerKey::new(session, "abcd2345").expect("valid worker key")
    }

    #[test]
    fn a_set_aside_definition_without_its_definition_is_listed() {
        let fixture = Fixture::new();
        let orphaned = fixture.label("s-1");
        fixture.plant(&set_aside_name(&orphaned), &document(&orphaned), 0o600);
        let settled = fixture.label("s-2");
        fixture.plant(&set_aside_name(&settled), &document(&settled), 0o600);
        fixture.plant(&format!("{settled}.plist"), &document(&settled), 0o600);
        let other = Namespace::parse("ba9876543210").expect("valid namespace");
        let foreign = other.worker_label(&key("s-3"));
        fixture.plant(&set_aside_name(&foreign), &document(&foreign), 0o600);
        fixture.plant(
            &set_aside_name(&fixture.namespace.daemon_label()),
            b"agent",
            0o600,
        );

        let entries = fixture.classify();

        assert_eq!(entries.candidates, vec![key("s-2")]);
        assert_eq!(entries.set_aside, vec![key("s-1")]);
        assert!(entries.rejected.is_empty(), "{:?}", entries.rejected);
    }

    #[test]
    fn a_set_aside_definition_is_listed_whatever_its_content() {
        let fixture = Fixture::new();
        let label = fixture.label("s-1");
        fixture.plant(&set_aside_name(&label), b"not a plist", 0o644);

        assert_eq!(fixture.classify().set_aside, vec![key("s-1")]);
    }

    /// Fake `launchctl` whose `print` exits with `status`; `$1` is the
    /// subcommand and `$2` the target.
    fn launchctl(script: &str) -> Launchctl {
        Launchctl::with_program(Path::new("/bin/sh"), &["-c", script, "launchctl"], DEADLINE)
    }

    fn set_aside_loaded_with(script: &str, keys: &[WorkerKey]) -> Result<bool, Error> {
        let namespace = Namespace::parse(NAMESPACE).expect("valid namespace");
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime starts")
            .block_on(set_aside_loaded(
                &launchctl(script),
                DOMAIN,
                &namespace,
                keys,
                "discover",
            ))
    }

    #[test]
    fn an_absent_set_aside_worker_does_not_hold_the_discovery() {
        let loaded =
            set_aside_loaded_with(r#"[ "$1" = print ] && exit 113; exit 1"#, &[key("s-1")])
                .expect("probe succeeds");
        assert!(!loaded);
    }

    #[test]
    fn a_loaded_set_aside_worker_makes_the_discovery_incomplete() {
        let namespace = Namespace::parse(NAMESPACE).expect("valid namespace");
        let target = format!("{DOMAIN}/{}", namespace.worker_label(&key("s-2")));
        let script =
            format!(r#"[ "$1" = print ] || exit 1; [ "$2" = "{target}" ] && exit 0; exit 113"#);
        let loaded =
            set_aside_loaded_with(&script, &[key("s-1"), key("s-2")]).expect("probe succeeds");
        assert!(loaded);
    }

    #[test]
    fn a_failed_set_aside_probe_fails_the_discovery() {
        let result = set_aside_loaded_with("exit 112", &[key("s-1")]);
        assert!(
            matches!(&result, Err(Error::DomainUnavailable { domain }) if domain == DOMAIN),
            "{result:?}"
        );
    }

    #[test]
    fn without_set_aside_workers_nothing_is_probed() {
        let loaded = set_aside_loaded_with("exit 1", &[]).expect("no probe runs");
        assert!(!loaded);
    }
}
