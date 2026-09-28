//! Publishes a job definition file and bootstraps it without losing the
//! definition it replaces.
//!
//! Both launchd backends register a job the same way: a leftover
//! `<label>.plist` of an absent label is moved to its set-aside name
//! `.<label>.plist.replaced`, the new definition is published under
//! `<label>.plist`, and `launchctl bootstrap` loads it. The leftover stays set
//! aside until the bootstrap result decides which file launchd may load:
//!
//! | Outcome | New definition | Leftover |
//! |---|---|---|
//! | nothing published | never written | put back |
//! | `bootstrap` succeeded, or `EIO` with the label loaded | kept | discarded |
//! | `EIO` with the label absent, or no such domain | removed | put back |
//! | any other status, a `launchctl` run failure, or a failed re-probe | kept | discarded |
//!
//! A leftover that cannot be put back is reported together with the original
//! failure as [`StrandedDefinition`]; it is never dropped silently.
//!
//! The set-aside name is derived from the label, so a registration interrupted
//! by a crash is settled by [`recover`] before the label's next registration,
//! replacement, retirement, or uninstallation: the set-aside file returns to
//! `<label>.plist` while that name is free and is discarded otherwise.
//!
//! The module only needs a [`Launchctl`] runner and a [`TrustedDir`], so it is
//! target-neutral and its tests run a fake `launchctl` on every host.

use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt as _;
use std::path::PathBuf;

use super::super::{Error, ServiceId};
use super::launchctl::{Launchctl, Status};
use super::{presence, run_error, status_error, Presence};
use crate::filesystem::{
    EntryKind, FsError, MoveOutcome, RemoveOutcome, StageOutcome, StagedEntry, TrustedDir,
};

// Rust guideline compliant 2026-09-28

/// Mode of every definition file; launchd refuses group- or world-writable plists.
pub(super) const DEFINITION_MODE: u32 = 0o600;

/// File-name suffix of every definition.
pub(super) const DEFINITION_SUFFIX: &str = ".plist";

/// Prefix of the temporary name an atomic definition write starts from.
///
/// Starts with `.` and never ends in [`DEFINITION_SUFFIX`], so discovery never
/// mistakes an interrupted write for a definition.
const TEMPORARY_PREFIX: &str = ".pohunek-launchd-";

/// Prefix of the quarantine name a removed file passes through.
///
/// Never ends in [`DEFINITION_SUFFIX`], so neither discovery nor launchd at
/// login reads a quarantined file as a definition.
const REMOVAL_PREFIX: &str = ".pohunek-launchd-removed-";

/// Suffix of the name a replaced definition waits under during [`register`].
///
/// The name is `.<label>.plist` followed by this suffix: hidden and never
/// ending in [`DEFINITION_SUFFIX`], so neither discovery nor launchd at login
/// reads it as a definition, and fixed per label, so [`recover`] finds it
/// after a crash.
const SET_ASIDE_SUFFIX: &str = ".replaced";

/// Random bytes in a temporary definition name.
const TEMPORARY_RANDOM_BYTES: usize = 8;

/// Mount point of the boot volume group's data volume.
///
/// Since macOS 10.15 the home directories on the boot disk live on this
/// volume (firmlinked into `/Users`), while `/` is the sealed system volume.
const BOOT_DATA_VOLUME: &str = "/System/Volumes/Data";

/// launchd refused a definition stored outside the boot volume group.
///
/// launchd does not load job definitions from external volumes. The backend
/// reports this instead of silently storing definitions somewhere else; it is
/// the source of an [`Error::Unavailable`] from `start` or `install`.
#[derive(Debug, thiserror::Error)]
#[error(
    "launchd refused the definition directory {path} because it is not on the boot volume",
    path = .path.display()
)]
pub struct ExternalVolume {
    /// The definition directory.
    pub path: PathBuf,
}

/// A registration failed and the definition it replaced could not be put back.
///
/// The replaced definition stays under its set-aside name
/// [`StrandedDefinition::path`], which neither launchd nor discovery reads.
/// The label's next `start`, `install`, `replace`, `retire`, or `uninstall`
/// settles it first: it returns to `<label>.plist` while that name is free and
/// is discarded when `<label>.plist` holds a definition. It is the source of an
/// [`Error::Operation`] from `start`, `install`, or `replace`.
#[derive(Debug, thiserror::Error)]
#[error(
    "{failure}; the replaced definition could not be put back and stays at {path}: {restore}",
    path = .path.display()
)]
pub struct StrandedDefinition {
    /// The registration failure, which is what the caller would otherwise see.
    pub failure: Error,
    /// Why the replaced definition could not be put back.
    pub restore: Error,
    /// Quarantine path of the replaced definition.
    pub path: PathBuf,
}

/// What a registration left under `<label>.plist`.
#[derive(Debug)]
enum Outcome {
    /// Nothing was published; the leftover must return to its name.
    Unpublished(Error),
    /// The published definition is, or may be, what launchd loaded, so it
    /// stays and the leftover is obsolete.
    Kept(Result<(), Error>),
    /// launchd proved it loaded nothing from the published definition, so it
    /// is removed and the leftover returns to its name.
    Refused(Error),
}

/// Returns the definition file name of `label`.
pub(super) fn definition_name(label: &str) -> String {
    format!("{label}{DEFINITION_SUFFIX}")
}

/// Returns the name the replaced definition of `label` waits under.
fn set_aside_name(label: &str) -> String {
    format!(".{label}{DEFINITION_SUFFIX}{SET_ASIDE_SUFFIX}")
}

/// Maps a trusted-filesystem failure of `operation`.
pub(super) fn fs_error(operation: &'static str) -> impl Fn(FsError) -> Error {
    move |error| Error::Operation {
        operation,
        source: Box::new(error),
    }
}

/// Requires the `<label>.plist` a replacement rewrites to exist in `directory`.
///
/// A missing file is [`Error::NotFound`], so a replacement never turns into a
/// fresh registration that hides a lost installation. The file must be a
/// private regular file as [`register`] writes it; a symlink, a hard-linked
/// file, or another owner or mode is refused. Its content is not parsed: a
/// damaged definition is what a replacement repairs.
pub(super) fn require_definition(
    directory: &TrustedDir,
    id: &ServiceId,
    label: &str,
    operation: &'static str,
) -> Result<(), Error> {
    match directory
        .entry_identity_with_mode(
            definition_name(label),
            EntryKind::RegularFile,
            DEFINITION_MODE,
        )
        .map_err(fs_error(operation))?
    {
        Some(_) => Ok(()),
        None => Err(Error::NotFound(id.clone())),
    }
}

/// Settles a replaced definition of `label` that an interrupted [`register`]
/// left set aside in `directory`.
///
/// While `<label>.plist` exists it is what launchd loads next, so the set-aside
/// file is obsolete and removed. Without `<label>.plist` the set-aside file is
/// the job's only definition and returns to that name. Neither branch replaces
/// an existing file, so no definition a loaded job was read from is
/// overwritten. Without a set-aside file nothing changes.
///
/// # Errors
///
/// Returns [`Error::Operation`] when either name is not a private regular
/// file or a move fails, and [`Error::Race`] when either name changes while
/// it is settled.
pub(super) fn recover(
    directory: &TrustedDir,
    label: &str,
    operation: &'static str,
) -> Result<(), Error> {
    let parked = set_aside_name(label);
    let Some(identity) = directory
        .entry_identity(&parked, EntryKind::RegularFile)
        .map_err(fs_error(operation))?
    else {
        return Ok(());
    };
    let file_name = definition_name(label);
    if directory
        .entry_identity(&file_name, EntryKind::RegularFile)
        .map_err(fs_error(operation))?
        .is_some()
    {
        remove_file(directory, &parked, operation)?;
        tracing::event!(
            name: "supervisor.register.set_aside_discarded",
            tracing::Level::INFO,
            supervisor.entry = %directory.path().join(&parked).display(),
            "removed the obsolete replaced definition {{supervisor.entry}}"
        );
        return Ok(());
    }
    match directory
        .stage(&parked, &file_name, identity)
        .map_err(fs_error(operation))?
    {
        StageOutcome::Staged(_) => {
            tracing::event!(
                name: "supervisor.register.set_aside_restored",
                tracing::Level::WARN,
                supervisor.entry = %directory.path().join(&file_name).display(),
                "restored the definition {{supervisor.entry}} an interrupted registration left set aside"
            );
            Ok(())
        }
        StageOutcome::Missing => Ok(()),
        _ => Err(Error::Race { operation }),
    }
}

/// Probes whether `label` is loaded in `domain` with `launchctl print`.
pub(super) async fn probe(
    launchctl: &Launchctl,
    domain: &str,
    label: &str,
    operation: &'static str,
) -> Result<Presence, Error> {
    let target = format!("{domain}/{label}");
    let completion = launchctl
        .run("print", &[OsStr::new(&target)])
        .await
        .map_err(|error| run_error(operation, error))?;
    presence(operation, domain, &completion)
}

/// Writes `<label>.plist` into `directory` and bootstraps it into `domain`.
///
/// The definition of a loaded label is never overwritten. launchd may load a
/// leftover `<label>.plist` on its own (the daemon agent at login), so the
/// leftover is moved aside and the label probed again before the new file is
/// published under a name that must not exist yet: a load racing this call
/// has then read either the leftover, which is put back, or the new file,
/// which is what the loaded job runs. The leftover stays set aside until
/// `bootstrap` decides which file remains; see the module documentation.
///
/// Every file operation is a single rename or removal that never replaces an
/// existing name, and the leftover waits under the fixed name
/// `.<label>.plist.replaced`. A crash therefore leaves one of three states,
/// each settled by [`recover`] before the next registration of the label:
///
/// - set aside, nothing published (or a refused definition already removed):
///   only the set-aside file exists, and it returns to `<label>.plist`;
/// - published, before or after `bootstrap`: both files exist, the published
///   definition is kept as launchd may have loaded it, and the set-aside file
///   is discarded, which is what an unknown `bootstrap` outcome does;
/// - settled: only `<label>.plist` exists, and nothing changes.
///
/// # Errors
///
/// Returns [`Error::AlreadyRegistered`] for a loaded label, the `bootstrap`
/// failure otherwise, and [`StrandedDefinition`] inside [`Error::Operation`]
/// when the leftover could not be put back.
pub(super) async fn register(
    launchctl: &Launchctl,
    domain: &str,
    operation: &'static str,
    id: &ServiceId,
    label: &str,
    directory: &TrustedDir,
    bytes: &[u8],
) -> Result<(), Error> {
    recover(directory, label, operation)?;
    if probe(launchctl, domain, label, operation).await? == Presence::Loaded {
        return Err(Error::AlreadyRegistered(id.clone()));
    }
    let file_name = definition_name(label);
    let parked = set_aside_name(label);
    let leftover = set_aside(directory, &file_name, &parked, operation)?;
    // Every path from here settles the leftover; nothing may return early.
    let outcome = match probe(launchctl, domain, label, operation).await {
        Ok(Presence::Absent) => match publish_definition(directory, &file_name, bytes, operation) {
            Ok(()) => {
                bootstrap(
                    launchctl, domain, operation, id, label, directory, &file_name,
                )
                .await
            }
            Err(error) => Outcome::Unpublished(error),
        },
        Ok(Presence::Loaded) => Outcome::Unpublished(Error::AlreadyRegistered(id.clone())),
        Err(error) => Outcome::Unpublished(error),
    };
    settle(directory, &file_name, &parked, leftover, outcome, operation)
}

/// Bootstraps the published `file_name` and classifies what launchd loaded.
async fn bootstrap(
    launchctl: &Launchctl,
    domain: &str,
    operation: &'static str,
    id: &ServiceId,
    label: &str,
    directory: &TrustedDir,
    file_name: &str,
) -> Outcome {
    let path = directory.path().join(file_name);
    let completion = match launchctl
        .run("bootstrap", &[OsStr::new(domain), path.as_os_str()])
        .await
    {
        Ok(completion) => completion,
        // Without an exit status launchd may have loaded the file anyway.
        Err(error) => return Outcome::Kept(Err(run_error(operation, error))),
    };
    match completion.status {
        Status::Success => Outcome::Kept(Ok(())),
        // `EIO` covers both an already loaded label and a definition launchd
        // refused to read; `print` tells them apart.
        Status::InputOutput => match probe(launchctl, domain, label, operation).await {
            Ok(Presence::Loaded) => Outcome::Kept(Err(Error::AlreadyRegistered(id.clone()))),
            Ok(Presence::Absent) => Outcome::Refused(match on_boot_volume(directory, operation) {
                Ok(true) => status_error(operation, domain, &completion),
                Ok(false) => Error::Unavailable {
                    operation,
                    source: Box::new(ExternalVolume {
                        path: directory.path().to_path_buf(),
                    }),
                },
                Err(error) => error,
            }),
            Err(error) => Outcome::Kept(Err(error)),
        },
        Status::NoSuchDomain => Outcome::Refused(status_error(operation, domain, &completion)),
        // The label's state is unknown, so the published definition stays for
        // discovery and retirement to reconcile, and the leftover it replaced
        // is obsolete either way.
        Status::NoSuchProcess | Status::InProgress | Status::NoSuchService | Status::Unmapped => {
            Outcome::Kept(Err(status_error(operation, domain, &completion)))
        }
    }
}

/// Applies `outcome` to the published definition and the set-aside leftover.
fn settle(
    directory: &TrustedDir,
    file_name: &str,
    parked: &str,
    leftover: Option<StagedEntry>,
    outcome: Outcome,
    operation: &'static str,
) -> Result<(), Error> {
    match outcome {
        Outcome::Kept(result) => {
            if let Some(leftover) = leftover {
                discard_obsolete(leftover, operation);
            }
            result
        }
        Outcome::Unpublished(failure) => Err(restore(
            directory, parked, leftover, file_name, failure, operation,
        )),
        Outcome::Refused(failure) => match remove_file(directory, file_name, operation) {
            Ok(()) => Err(restore(
                directory, parked, leftover, file_name, failure, operation,
            )),
            // The occupied name leaves no room to put the leftover back.
            Err(removal) => Err(match leftover {
                Some(leftover) => stranded(&leftover, failure, removal, operation),
                None => failure,
            }),
        },
    }
}

/// Puts `leftover` back under `file_name` and returns `failure`, combined with
/// the restore error when that fails.
fn restore(
    directory: &TrustedDir,
    parked: &str,
    leftover: Option<StagedEntry>,
    file_name: &str,
    failure: Error,
    operation: &'static str,
) -> Error {
    let Some(leftover) = leftover else {
        return failure;
    };
    match put_back(directory, parked, &leftover, file_name, operation) {
        Ok(()) => failure,
        Err(restore) => stranded(&leftover, failure, restore, operation),
    }
}

fn stranded(
    leftover: &StagedEntry,
    failure: Error,
    restore: Error,
    operation: &'static str,
) -> Error {
    Error::Operation {
        operation,
        source: Box::new(StrandedDefinition {
            failure,
            restore,
            path: leftover.path(),
        }),
    }
}

/// Deletes a leftover the kept definition replaced.
///
/// A failure is logged instead of failing a registration that already loaded
/// or kept the new definition: the set-aside name is never read as a
/// definition, and [`recover`] removes the leftover before the label's next
/// registration.
fn discard_obsolete(leftover: StagedEntry, operation: &'static str) {
    let path = leftover.path();
    if let Err(error) = discard(leftover, operation) {
        tracing::event!(
            name: "supervisor.register.discard_failed",
            tracing::Level::WARN,
            supervisor.entry = %path.display(),
            error.message = %error,
            "could not remove the replaced definition {{supervisor.entry}}"
        );
    }
}

/// Publishes a `0600` definition under `file_name`, which must not exist.
///
/// The file is written and synchronized under a random temporary name first,
/// so launchd never reads a partial definition. An occupied `file_name` is
/// [`Error::Race`] and leaves the existing file untouched.
fn publish_definition(
    directory: &TrustedDir,
    file_name: &str,
    bytes: &[u8],
    operation: &'static str,
) -> Result<(), Error> {
    let mut random = [0_u8; TEMPORARY_RANDOM_BYTES];
    getrandom::getrandom(&mut random).map_err(|error| Error::Unavailable {
        operation,
        source: Box::new(std::io::Error::other(error.to_string())),
    })?;
    let mut temporary = String::from(TEMPORARY_PREFIX);
    for byte in random {
        use std::fmt::Write as _;
        write!(temporary, "{byte:02x}").expect("writing hexadecimal to String cannot fail");
    }
    temporary.push_str(".tmp");
    directory
        .create_file(&temporary, bytes, DEFINITION_MODE)
        .map_err(fs_error(operation))?;
    match directory.move_no_replace(&temporary, directory, file_name) {
        Ok(MoveOutcome::Moved) => Ok(()),
        Ok(_) => {
            remove_file(directory, &temporary, operation)?;
            Err(Error::Race { operation })
        }
        Err(error) => {
            // The move failure is the error to report; a temporary file that
            // cannot be removed as well is never read as a definition.
            if let Err(cleanup) = remove_file(directory, &temporary, operation) {
                tracing::event!(
                    name: "supervisor.register.cleanup_failed",
                    tracing::Level::WARN,
                    supervisor.entry = temporary.as_str(),
                    error.message = %cleanup,
                    "could not remove the temporary definition {{supervisor.entry}}"
                );
            }
            Err(fs_error(operation)(error))
        }
    }
}

/// Moves an existing definition file to the set-aside name `parked`; a
/// missing file is `None`.
///
/// An occupied `parked` is [`Error::Race`]: [`recover`] has settled it before.
fn set_aside(
    directory: &TrustedDir,
    file_name: &str,
    parked: &str,
    operation: &'static str,
) -> Result<Option<StagedEntry>, Error> {
    let Some(identity) = directory
        .entry_identity(file_name, EntryKind::RegularFile)
        .map_err(fs_error(operation))?
    else {
        return Ok(None);
    };
    match directory
        .stage(file_name, parked, identity)
        .map_err(fs_error(operation))?
    {
        StageOutcome::Staged(entry) => Ok(Some(entry)),
        StageOutcome::Missing => Ok(None),
        _ => Err(Error::Race { operation }),
    }
}

/// Restores a definition moved aside by [`set_aside`] under `parked`.
///
/// One identity-checked rename that never replaces `file_name`, so a crash
/// leaves the definition under either name, where [`recover`] finds it.
fn put_back(
    directory: &TrustedDir,
    parked: &str,
    entry: &StagedEntry,
    file_name: &str,
    operation: &'static str,
) -> Result<(), Error> {
    match directory
        .stage(parked, file_name, entry.identity())
        .map_err(fs_error(operation))?
    {
        StageOutcome::Staged(_) => Ok(()),
        _ => Err(Error::Race { operation }),
    }
}

/// Deletes a definition moved aside by [`set_aside`].
fn discard(entry: StagedEntry, operation: &'static str) -> Result<(), Error> {
    match entry.remove().map_err(fs_error(operation))? {
        RemoveOutcome::Removed | RemoveOutcome::Missing => Ok(()),
        _ => Err(Error::Race { operation }),
    }
}

/// Removes one regular file bound to the inode that was inspected.
pub(super) fn remove_file(
    directory: &TrustedDir,
    file_name: &str,
    operation: &'static str,
) -> Result<(), Error> {
    let Some(identity) = directory
        .entry_identity(file_name, EntryKind::RegularFile)
        .map_err(fs_error(operation))?
    else {
        return Ok(());
    };
    match directory
        .stage_random(file_name, REMOVAL_PREFIX, identity)
        .map_err(fs_error(operation))?
    {
        StageOutcome::Staged(entry) => match entry.remove().map_err(fs_error(operation))? {
            RemoveOutcome::Removed | RemoveOutcome::Missing => Ok(()),
            _ => Err(Error::Race { operation }),
        },
        StageOutcome::Missing => Ok(()),
        _ => Err(Error::Race { operation }),
    }
}

/// Returns whether `directory` is on the boot volume group.
fn on_boot_volume(directory: &TrustedDir, operation: &'static str) -> Result<bool, Error> {
    let io = |source: std::io::Error| Error::Operation {
        operation,
        source: Box::new(source),
    };
    let device = directory
        .try_clone_descriptor()
        .map_err(fs_error(operation))?
        .metadata()
        .map_err(io)?
        .dev();
    for root in ["/", BOOT_DATA_VOLUME] {
        match std::fs::metadata(root) {
            Ok(metadata) if metadata.dev() == device => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(error)),
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;
    use std::time::Duration;

    use super::super::plist::MAX_DEFINITION_BYTES;
    use super::*;

    const DOMAIN: &str = "gui/501";

    const LABEL: &str = "io.github.zajca.pohunek.test.worker.s-1.abcd2345";

    /// Deadline of one fake `launchctl` command.
    ///
    /// Long enough for `sh` to start on a loaded runner; the timeout test
    /// waits for it once.
    const DEADLINE: Duration = Duration::from_secs(2);

    /// Fake `launchctl` whose `print` reports the label absent.
    ///
    /// `$1` is the per-test marker path, `$2` the subcommand; `bootstrap`
    /// exits with the status the test inserts for `STATUS`.
    const ABSENT_THEN: &str =
        r#"case "$2" in print) exit 113;; bootstrap) exit STATUS;; esac; exit 1"#;

    struct Fixture {
        _root: tempfile::TempDir,
        directory: TrustedDir,
        marker: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("temporary directory");
            let path = std::fs::canonicalize(root.path()).expect("canonical temporary directory");
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o700))
                .expect("private mode");
            let definitions = path.join("definitions");
            let directory = TrustedDir::open_or_create_absolute(&definitions, 0o700)
                .expect("private directory");
            Self {
                _root: root,
                directory,
                marker: path.join("marker"),
            }
        }

        fn launchctl(&self, script: &str) -> Launchctl {
            Launchctl::with_program(
                Path::new("/bin/sh"),
                &[
                    "-c",
                    script,
                    "launchctl",
                    self.marker.to_str().expect("UTF-8 marker path"),
                ],
                DEADLINE,
            )
        }

        fn plant_leftover(&self) {
            publish_definition(&self.directory, &definition_name(LABEL), b"old", "start")
                .expect("leftover planted");
        }

        async fn register(&self, script: &str) -> Result<(), Error> {
            let id = ServiceId::parse(LABEL).expect("valid service id");
            register(
                &self.launchctl(script),
                DOMAIN,
                "start",
                &id,
                LABEL,
                &self.directory,
                b"new",
            )
            .await
        }

        fn names(&self) -> Vec<OsString> {
            names(&self.directory)
        }

        /// Leaves the state of a registration that crashed right after
        /// setting the planted leftover aside.
        fn crash_after_set_aside(&self) {
            let leftover = set_aside(
                &self.directory,
                &definition_name(LABEL),
                &set_aside_name(LABEL),
                "start",
            )
            .expect("set aside")
            .expect("leftover exists");
            drop(leftover);
            assert_eq!(self.names(), vec![OsString::from(set_aside_name(LABEL))]);
        }

        /// Runs the file steps of a daemon `replace` after its `bootout`.
        async fn replace(&self, script: &str) -> Result<(), Error> {
            let id = ServiceId::parse(LABEL).expect("valid service id");
            recover(&self.directory, LABEL, "replace")?;
            require_definition(&self.directory, &id, LABEL, "replace")?;
            self.register(script).await
        }

        fn definition(&self) -> Vec<u8> {
            self.directory
                .read_file(
                    definition_name(LABEL),
                    DEFINITION_MODE,
                    MAX_DEFINITION_BYTES,
                )
                .expect("definition")
        }
    }

    fn names(directory: &TrustedDir) -> Vec<OsString> {
        let mut names = directory.entry_names().expect("directory listing");
        names.sort();
        names
    }

    fn only_definition() -> Vec<OsString> {
        vec![OsString::from(definition_name(LABEL))]
    }

    fn absent_then(status: &str) -> String {
        ABSENT_THEN.replace("STATUS", status)
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime starts")
    }

    #[test]
    fn a_successful_bootstrap_keeps_only_the_new_definition() {
        let fixture = Fixture::new();
        fixture.plant_leftover();
        runtime()
            .block_on(fixture.register(&absent_then("0")))
            .expect("registered");
        assert_eq!(fixture.names(), only_definition());
        assert_eq!(fixture.definition(), b"new");
    }

    #[test]
    fn a_missing_domain_puts_the_replaced_definition_back() {
        let fixture = Fixture::new();
        fixture.plant_leftover();
        let result = runtime().block_on(fixture.register(&absent_then("112")));
        assert!(
            matches!(&result, Err(Error::DomainUnavailable { domain }) if domain == DOMAIN),
            "{result:?}"
        );
        assert_eq!(fixture.names(), only_definition());
        assert_eq!(fixture.definition(), b"old");
    }

    #[test]
    fn a_missing_domain_without_a_leftover_leaves_no_definition() {
        let fixture = Fixture::new();
        let result = runtime().block_on(fixture.register(&absent_then("112")));
        assert!(
            matches!(result, Err(Error::DomainUnavailable { .. })),
            "{result:?}"
        );
        assert!(fixture.names().is_empty());
    }

    #[test]
    fn a_refused_definition_puts_the_replaced_definition_back() {
        let fixture = Fixture::new();
        fixture.plant_leftover();
        // Whether the refusal reads as an external volume depends on the
        // host's temporary directory; the files do not.
        let result = runtime().block_on(fixture.register(&absent_then("5")));
        assert!(
            matches!(
                &result,
                Err(Error::Operation {
                    operation: "start",
                    ..
                } | Error::Unavailable {
                    operation: "start",
                    ..
                })
            ),
            "{result:?}"
        );
        assert_eq!(fixture.names(), only_definition());
        assert_eq!(fixture.definition(), b"old");
    }

    #[test]
    fn an_already_loaded_label_after_eio_keeps_the_new_definition() {
        let fixture = Fixture::new();
        fixture.plant_leftover();
        // `bootstrap` loads the label (the marker) and still reports `EIO`.
        let script = r#"case "$2" in
            print) [ -e "$1" ] && exit 0; exit 113;;
            bootstrap) : > "$1"; exit 5;;
        esac; exit 1"#;
        let result = runtime().block_on(fixture.register(script));
        assert!(
            matches!(result, Err(Error::AlreadyRegistered(_))),
            "{result:?}"
        );
        assert_eq!(fixture.names(), only_definition());
        assert_eq!(fixture.definition(), b"new");
    }

    #[test]
    fn an_unknown_status_keeps_the_new_definition() {
        let fixture = Fixture::new();
        fixture.plant_leftover();
        let result = runtime().block_on(fixture.register(&absent_then("42")));
        assert!(
            matches!(
                result,
                Err(Error::Operation {
                    operation: "start",
                    ..
                })
            ),
            "{result:?}"
        );
        assert_eq!(fixture.names(), only_definition());
        assert_eq!(fixture.definition(), b"new");
    }

    #[test]
    fn a_bootstrap_without_exit_status_keeps_the_new_definition() {
        let fixture = Fixture::new();
        fixture.plant_leftover();
        let script =
            r#"case "$2" in print) exit 113;; bootstrap) exec /bin/sleep 30;; esac; exit 1"#;
        let result = runtime().block_on(fixture.register(script));
        assert!(
            matches!(result, Err(Error::Timeout { operation: "start" })),
            "{result:?}"
        );
        assert_eq!(fixture.names(), only_definition());
        assert_eq!(fixture.definition(), b"new");
    }

    #[test]
    fn a_label_loaded_before_publishing_puts_the_leftover_back() {
        let fixture = Fixture::new();
        fixture.plant_leftover();
        // The first probe finds the label absent and loads it (the marker),
        // so the probe after setting the leftover aside finds it loaded.
        let script = r#"case "$2" in
            print) [ -e "$1" ] && exit 0; : > "$1"; exit 113;;
        esac; exit 1"#;
        let result = runtime().block_on(fixture.register(script));
        assert!(
            matches!(result, Err(Error::AlreadyRegistered(_))),
            "{result:?}"
        );
        assert_eq!(fixture.names(), only_definition());
        assert_eq!(fixture.definition(), b"old");
    }

    #[test]
    fn a_refused_definition_that_cannot_be_removed_strands_the_leftover_visibly() {
        let fixture = Fixture::new();
        fixture.plant_leftover();
        let leftover = set_aside(
            &fixture.directory,
            &definition_name(LABEL),
            &set_aside_name(LABEL),
            "start",
        )
        .expect("set aside")
        .expect("leftover exists");
        let stranded_path = leftover.path();
        // A directory under the definition name is not a regular file, so the
        // new definition cannot be removed and the name stays occupied.
        std::fs::create_dir(fixture.directory.path().join(definition_name(LABEL)))
            .expect("occupying directory");
        let result = settle(
            &fixture.directory,
            &definition_name(LABEL),
            &set_aside_name(LABEL),
            Some(leftover),
            Outcome::Refused(Error::DomainUnavailable {
                domain: DOMAIN.to_owned(),
            }),
            "start",
        );
        let Err(Error::Operation { source, .. }) = result else {
            panic!("a stranded leftover must be an operation error: {result:?}");
        };
        let stranded = source
            .downcast_ref::<StrandedDefinition>()
            .expect("stranded definition source");
        assert!(matches!(stranded.failure, Error::DomainUnavailable { .. }));
        assert_eq!(stranded.path, stranded_path);
        assert!(stranded_path.exists(), "the leftover is never deleted");
    }

    #[test]
    fn a_replace_after_a_crash_following_the_set_aside_restores_and_succeeds() {
        let fixture = Fixture::new();
        fixture.plant_leftover();
        fixture.crash_after_set_aside();
        runtime()
            .block_on(fixture.replace(&absent_then("0")))
            .expect("replaced");
        assert_eq!(fixture.names(), only_definition());
        assert_eq!(fixture.definition(), b"new");
    }

    #[test]
    fn a_crash_following_the_set_aside_is_recovered_before_a_refusal() {
        let fixture = Fixture::new();
        fixture.plant_leftover();
        fixture.crash_after_set_aside();
        let result = runtime().block_on(fixture.register(&absent_then("112")));
        assert!(
            matches!(result, Err(Error::DomainUnavailable { .. })),
            "{result:?}"
        );
        assert_eq!(fixture.names(), only_definition());
        assert_eq!(fixture.definition(), b"old");
    }

    #[test]
    fn a_loaded_label_gets_its_set_aside_definition_back() {
        let fixture = Fixture::new();
        fixture.plant_leftover();
        fixture.crash_after_set_aside();
        let script = r#"case "$2" in print) exit 0;; esac; exit 1"#;
        let result = runtime().block_on(fixture.register(script));
        assert!(
            matches!(result, Err(Error::AlreadyRegistered(_))),
            "{result:?}"
        );
        assert_eq!(fixture.names(), only_definition());
        assert_eq!(fixture.definition(), b"old");
    }

    #[test]
    fn a_crash_following_the_publish_keeps_the_published_definition() {
        let fixture = Fixture::new();
        fixture.plant_leftover();
        fixture.crash_after_set_aside();
        publish_definition(
            &fixture.directory,
            &definition_name(LABEL),
            b"published",
            "start",
        )
        .expect("published");

        recover(&fixture.directory, LABEL, "replace").expect("recovered");
        assert_eq!(fixture.names(), only_definition());
        assert_eq!(fixture.definition(), b"published");

        runtime()
            .block_on(fixture.replace(&absent_then("0")))
            .expect("replaced");
        assert_eq!(fixture.names(), only_definition());
        assert_eq!(fixture.definition(), b"new");
    }

    #[test]
    fn recovery_without_a_set_aside_definition_changes_nothing() {
        let fixture = Fixture::new();
        let id = ServiceId::parse(LABEL).expect("valid service id");
        recover(&fixture.directory, LABEL, "replace").expect("nothing to recover");
        assert!(fixture.names().is_empty());
        let missing = require_definition(&fixture.directory, &id, LABEL, "replace");
        assert!(
            matches!(&missing, Err(Error::NotFound(found)) if *found == id),
            "{missing:?}"
        );
        let replaced = runtime().block_on(fixture.replace(&absent_then("0")));
        assert!(
            matches!(&replaced, Err(Error::NotFound(found)) if *found == id),
            "{replaced:?}"
        );
        assert!(fixture.names().is_empty());

        fixture.plant_leftover();
        recover(&fixture.directory, LABEL, "replace").expect("nothing to recover");
        assert_eq!(fixture.names(), only_definition());
        assert_eq!(fixture.definition(), b"old");
    }

    #[test]
    fn the_set_aside_name_is_never_a_definition_name() {
        let parked = set_aside_name(LABEL);
        assert!(parked.starts_with('.'), "{parked}");
        assert!(!parked.ends_with(DEFINITION_SUFFIX), "{parked}");
        assert_ne!(parked, definition_name(LABEL));
    }

    #[test]
    fn publishing_never_replaces_an_existing_definition() {
        let fixture = Fixture::new();
        let directory = &fixture.directory;
        publish_definition(directory, "job.plist", b"first", "start").expect("publish");
        assert!(matches!(
            publish_definition(directory, "job.plist", b"second", "start"),
            Err(Error::Race { operation: "start" })
        ));
        assert_eq!(
            directory
                .read_file("job.plist", DEFINITION_MODE, MAX_DEFINITION_BYTES)
                .expect("definition"),
            b"first"
        );
        assert_eq!(names(directory), vec![OsString::from("job.plist")]);
    }

    #[test]
    fn a_leftover_set_aside_is_put_back_or_discarded() {
        let fixture = Fixture::new();
        let directory = &fixture.directory;
        let parked = ".job.plist.replaced";
        assert!(set_aside(directory, "job.plist", parked, "start")
            .expect("missing leftover")
            .is_none());

        publish_definition(directory, "job.plist", b"leftover", "start").expect("publish");
        let leftover = set_aside(directory, "job.plist", parked, "start")
            .expect("set aside")
            .expect("leftover exists");
        assert_eq!(names(directory), vec![OsString::from(parked)]);
        put_back(directory, parked, &leftover, "job.plist", "start").expect("put back");
        assert_eq!(
            directory
                .read_file("job.plist", DEFINITION_MODE, MAX_DEFINITION_BYTES)
                .expect("definition"),
            b"leftover"
        );

        let leftover = set_aside(directory, "job.plist", parked, "start")
            .expect("set aside")
            .expect("leftover exists");
        publish_definition(directory, "job.plist", b"new", "start").expect("publish");
        discard(leftover, "start").expect("discard");
        assert_eq!(names(directory), vec![OsString::from("job.plist")]);
        assert_eq!(
            directory
                .read_file("job.plist", DEFINITION_MODE, MAX_DEFINITION_BYTES)
                .expect("definition"),
            b"new"
        );
    }

    #[test]
    fn a_replacement_requires_an_existing_private_definition() {
        let fixture = Fixture::new();
        let directory = &fixture.directory;
        let id = ServiceId::parse(LABEL).expect("valid service id");
        let path = directory.path().join(definition_name(LABEL));
        let require = || require_definition(directory, &id, LABEL, "replace");

        let missing = require();
        assert!(
            matches!(&missing, Err(Error::NotFound(found)) if *found == id),
            "{missing:?}"
        );

        // A damaged but private definition is what a replacement repairs.
        fixture.plant_leftover();
        require().expect("private definition");

        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o620))
            .expect("group-writable mode");
        let unsafe_mode = require();
        assert!(
            matches!(
                unsafe_mode,
                Err(Error::Operation {
                    operation: "replace",
                    ..
                })
            ),
            "{unsafe_mode:?}"
        );

        std::fs::remove_file(&path).expect("remove definition");
        let target = directory.path().join("target.plist");
        std::fs::write(&target, b"target").expect("symlink target");
        std::fs::set_permissions(&target, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .expect("private target");
        std::os::unix::fs::symlink(&target, &path).expect("symlinked definition");
        let symlink = require();
        assert!(
            matches!(
                symlink,
                Err(Error::Operation {
                    operation: "replace",
                    ..
                })
            ),
            "{symlink:?}"
        );

        std::fs::remove_file(&path).expect("remove symlink");
        std::fs::hard_link(&target, &path).expect("hard-linked definition");
        let hard_link = require();
        assert!(
            matches!(
                hard_link,
                Err(Error::Operation {
                    operation: "replace",
                    ..
                })
            ),
            "{hard_link:?}"
        );
    }

    // Only macOS guarantees a temporary directory on the boot volume.
    #[cfg(target_os = "macos")]
    #[test]
    fn private_directories_are_on_the_boot_volume() {
        let fixture = Fixture::new();
        assert!(on_boot_volume(&fixture.directory, "test").expect("volume check"));
    }
}
