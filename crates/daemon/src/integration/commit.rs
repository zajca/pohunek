//! Rollback-capable commit of one integration install or removal.
//!
//! An install rewrites several files (hook scripts first, provider
//! registration last); a removal edits the registration first and deletes the
//! owned scripts last. The ordered set is one transaction. Every replaced or
//! removed original is moved aside atomically into a private quarantine name
//! bound to its inode, verified there against what the caller decided on
//! (identity, mode, and complete content), and only deleted in a post-commit
//! cleanup phase once every step succeeded. A failing step therefore restores
//! each original by moving the very same inode back without replacing anything,
//! and a foreign change made at any moment is either seen on the moved inode
//! (`integration_destination_collision`) or blocks the no-replace activation.
//! A rollback that cannot restore a file is `integration_recovery_required`,
//! distinct from an unsafe path (`integration_path_untrusted`). A cleanup that
//! cannot finish never undoes a committed transaction: it is reported as
//! incomplete with the quarantine paths that remain.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use pohunek_platform::filesystem::{
    DestinationExpectation, DisplacingReplaceError, EntryIdentity, EntryKind, FsError, StagedEntry,
};
use protocol::{ErrorClass, ProtocolError};

use super::quarantine::{self, Settled, Staging};

use super::{
    committed_durability_error, io_error, path_untrusted, platform_path_error, LoadedFile,
    TrustedDir, PROVIDER_CONFIG_INSPECTION_LIMIT_BYTES, UNIX_MODE_MASK,
};

/// Quarantine prefix for an entry displaced by an install step.
const DISPLACED_PREFIX: &str = ".pohunek-integration-displaced-";

/// Quarantine prefix for a file written by a step that is being rolled back.
const ROLLBACK_QUARANTINE_PREFIX: &str = ".pohunek-integration-rollback-";

/// Largest script read to check its ownership marker.
const SNAPSHOT_LIMIT_BYTES: usize = PROVIDER_CONFIG_INSPECTION_LIMIT_BYTES;

/// Error code for a destination that changed while the installer ran.
pub(super) const DESTINATION_COLLISION_CODE: &str = "integration_destination_collision";

/// Error code for a rollback that could not restore every committed file.
pub(super) const RECOVERY_REQUIRED_CODE: &str = "integration_recovery_required";

/// Test race hook: called with a label and the step's destination name.
#[cfg(test)]
type RaceHookFn = Box<dyn FnMut(&str, &str)>;

#[cfg(test)]
thread_local! {
    /// Test seam: runs with a label and the step's destination name at the
    /// points between a decision and its action.
    pub(super) static RACE_HOOK: std::cell::RefCell<Option<RaceHookFn>> =
        std::cell::RefCell::new(None);
}

/// Runs the test race hook, if one is installed, at a decision-to-action gap.
#[cfg(test)]
pub(super) fn race_point(label: &str, name: &str) {
    // The hook is taken out while it runs so it may itself run an operation
    // that reaches further race points.
    RACE_HOOK.with(|slot| {
        let taken = slot.borrow_mut().take();
        if let Some(mut hook) = taken {
            hook(label, name);
            let mut slot = slot.borrow_mut();
            if slot.is_none() {
                *slot = Some(hook);
            }
        }
    });
}

#[cfg(not(test))]
pub(super) fn race_point(_label: &str, _name: &str) {}

/// Which command a transaction serves, so its messages name the right one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Operation {
    /// `pohunek integration install`.
    Install,
    /// `pohunek integration uninstall`.
    Uninstall,
}

impl Operation {
    /// The command word used in messages and hints.
    pub(super) const fn verb(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Uninstall => "uninstall",
        }
    }
}

/// Callback run before each step with the step index and destination name.
///
/// A returned error aborts the transaction exactly like a failing write.
pub(super) type StepGate<'g> = &'g mut dyn FnMut(usize, &str) -> Result<(), ProtocolError>;

/// What the caller already knows about a destination.
#[derive(Debug, Clone, Copy)]
pub(super) enum Expected<'a> {
    /// Fully managed file: any prior content may be overwritten.
    Any,
    /// Provider file merged from this state (`None` when it was absent). A
    /// destination whose content, mode, or inode differs at write time is a
    /// collision, never clobbered, and the replacement itself is bound to that
    /// inode.
    Loaded(Option<&'a LoadedFile>),
}

/// What one step does to its destination.
#[derive(Debug, Clone, Copy)]
pub(super) enum Action<'a> {
    /// Atomically replace the destination with `body` at `mode`.
    Write { body: &'a str, mode: u32 },
    /// Delete the destination only when it is a regular file carrying `marker`
    /// on a line of its own; anything else is left untouched.
    RemoveOwned { marker: &'a str },
    /// Write nothing: only confirm that a loaded input is still exactly what
    /// was read (`Expected::Loaded`), so a decision that left it unchanged is
    /// still valid at this point of the transaction.
    Verify,
}

/// What a committed step did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    /// The destination now holds the step's body.
    Written,
    /// The owned destination was deleted.
    Removed,
    /// The destination did not exist, so there was nothing to remove.
    Absent,
    /// The destination is not owned by the installer and was preserved.
    Foreign,
    /// A loaded input was confirmed unchanged.
    Verified,
}

/// One file change inside a transaction.
#[derive(Debug)]
pub(super) struct Step<'a> {
    pub(super) dir: &'a TrustedDir,
    pub(super) name: &'static str,
    pub(super) label: &'static str,
    pub(super) action: Action<'a>,
    pub(super) expected: Expected<'a>,
}

struct Applied<'a> {
    step: &'a Step<'a>,
    /// The original moved aside by this step, held until the commit is complete.
    displaced: Option<StagedEntry>,
    written: Option<EntryIdentity>,
    /// What the step wrote, kept to re-verify the file before anything
    /// destructive follows; `None` for a removal.
    content: Option<Written>,
    /// The file was changed by another writer after this step wrote it; that
    /// version is kept and the rollback never touches it.
    kept: bool,
    /// The step deleted the destination instead of writing it.
    removal: bool,
}

/// The exact bytes and mode a write step put in place.
struct Written {
    bytes: Vec<u8>,
    mode: u32,
}

/// The result of a committed transaction.
#[derive(Debug)]
pub(super) struct Committed {
    /// What each step did, in step order.
    pub(super) outcomes: Vec<Outcome>,
    /// Quarantined originals whose deletion did not finish, one entry each
    /// with the quarantine path and why. The transaction itself is committed.
    pub(super) cleanup_incomplete: Vec<String>,
}

/// Fault injected into the post-commit cleanup of one displaced original.
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub(super) enum CleanupFault {
    /// The unlink fails: the original stays under its quarantine name.
    Unlink,
    /// The unlink happens but the directory synchronization fails.
    DirectorySync,
}

#[cfg(test)]
type CleanupFaultFn = Box<dyn FnMut(&str) -> Option<CleanupFault>>;

#[cfg(test)]
thread_local! {
    /// Test seam: decides a cleanup fault from the step's destination name.
    pub(super) static CLEANUP_FAULT: std::cell::RefCell<Option<CleanupFaultFn>> =
        std::cell::RefCell::new(None);
}

/// Commits `steps` in order, rolling back on the first failure.
///
/// # Errors
///
/// Returns the failing step's error after a complete rollback, or
/// [`RECOVERY_REQUIRED_CODE`] when the rollback itself could not finish.
pub(super) fn commit(
    steps: &[Step<'_>],
    gate: StepGate<'_>,
    operation: Operation,
) -> Result<Committed, ProtocolError> {
    let mut applied: Vec<Applied<'_>> = Vec::with_capacity(steps.len());
    let mut outcomes = Vec::with_capacity(steps.len());
    for (index, step) in steps.iter().enumerate() {
        // Everything written so far must still be exactly what was written
        // before an asset is destroyed on the strength of it.
        if matches!(step.action, Action::RemoveOwned { .. }) {
            race_point("commit.before_removal", step.name);
            if let Err(error) = verify_written(&mut applied, operation, None) {
                return Err(roll_back(applied, error, operation));
            }
        }
        match run_step(index, step, gate, &mut applied, operation) {
            Ok(outcome) => outcomes.push(outcome),
            Err(error) => return Err(abort(applied, error, operation)),
        }
    }
    race_point("commit.before_finalize", "");
    if let Err(error) = verify_written(&mut applied, operation, None) {
        return Err(roll_back(applied, error, operation));
    }
    race_point("commit.committed", "");
    Ok(Committed {
        outcomes,
        cleanup_incomplete: clean_up(applied),
    })
}

/// Deletes every displaced original after the transaction committed.
///
/// Every item is attempted even after one fails, and nothing here can undo the
/// committed steps.
fn clean_up(applied: Vec<Applied<'_>>) -> Vec<String> {
    let mut incomplete = Vec::new();
    for entry in applied {
        let Some(staged) = entry.displaced else {
            continue;
        };
        let quarantine = staged.path();
        if let Some(problem) = discard_displaced(entry.step.name, staged) {
            incomplete.push(format!("{}: {problem}", quarantine.display()));
        }
    }
    incomplete
}

/// Deletes one displaced original; returns why it is not fully gone.
fn discard_displaced(name: &str, staged: StagedEntry) -> Option<String> {
    #[cfg(test)]
    {
        let fault = CLEANUP_FAULT.with(|slot| slot.borrow_mut().as_mut().and_then(|f| f(name)));
        match fault {
            Some(CleanupFault::Unlink) => {
                return Some("left behind (injected unlink failure)".to_owned())
            }
            Some(CleanupFault::DirectorySync) => {
                let _ = staged.remove();
                return Some(
                    "removed but not confirmed durable (injected sync failure)".to_owned(),
                );
            }
            None => {}
        }
    }
    #[cfg(not(test))]
    let _ = name;
    match quarantine::discard(staged) {
        Settled::Done => None,
        Settled::Unconfirmed(note) => Some(format!("removed but not confirmed durable ({note})")),
        Settled::Left { at, why } => Some(format!("left behind at {} ({why})", at.display())),
    }
}

/// How a write step treats its destination, decided before any mutation.
struct WritePlan<'a> {
    expectation: DestinationExpectation<'a>,
    /// A symlink moved aside first, so the replacement never follows it.
    symlink: Option<StagedEntry>,
}

fn plan_write<'a>(step: &Step<'a>, operation: Operation) -> Result<WritePlan<'a>, ProtocolError> {
    let path = step.dir.path.join(step.name);
    if let Expected::Loaded(before) = step.expected {
        let expectation = match before {
            None => DestinationExpectation::Absent,
            Some(file) => DestinationExpectation::Exact {
                identity: file.identity,
                mode: file.mode,
                content: file.content.as_bytes(),
            },
        };
        return Ok(WritePlan {
            expectation,
            symlink: None,
        });
    }
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(WritePlan {
            expectation: DestinationExpectation::Any,
            symlink: None,
        }),
        Err(error) => Err(io_error("inspect integration destination", &path, &error)),
        Ok(metadata) if metadata.file_type().is_symlink() => Ok(WritePlan {
            expectation: DestinationExpectation::Absent,
            symlink: Some(stage_displaced(step, EntryKind::Symlink, operation)?),
        }),
        Ok(metadata) if metadata.is_file() => Ok(WritePlan {
            expectation: DestinationExpectation::Any,
            symlink: None,
        }),
        Ok(_metadata) => Err(path_untrusted(
            &path,
            "managed path is neither a regular file nor a symlink",
        )),
    }
}

fn run_step<'a>(
    index: usize,
    step: &'a Step<'a>,
    gate: StepGate<'_>,
    applied: &mut Vec<Applied<'a>>,
    operation: Operation,
) -> Result<Outcome, ProtocolError> {
    gate(index, step.name)?;
    let (body, mode) = match step.action {
        Action::Write { body, mode } => (body, mode),
        Action::RemoveOwned { marker } => {
            return remove_owned(step, marker, applied, operation);
        }
        Action::Verify => return verify_unchanged(step, operation),
    };
    let plan = plan_write(step, operation)?;
    race_point("write.decided", step.name);
    let path = step.dir.path.join(step.name);
    let temp_name = format!(".{}.{}.tmp", step.name, ulid::Ulid::new());
    let result = step.dir.trusted.replace_file_displacing(
        step.name,
        temp_name,
        body.as_bytes(),
        mode,
        plan.expectation,
    );
    match result {
        Ok(replaced) => {
            applied.push(Applied {
                step,
                displaced: plan.symlink.or(replaced.displaced),
                written: Some(replaced.written),
                content: Some(Written {
                    bytes: body.as_bytes().to_vec(),
                    mode,
                }),
                kept: false,
                removal: false,
            });
            race_point("write.committed", step.name);
            Ok(Outcome::Written)
        }
        Err(DisplacingReplaceError::CommittedDurabilityUncertain {
            source,
            displaced,
            written,
        }) => {
            applied.push(Applied {
                step,
                displaced: plan.symlink.or(displaced.map(|boxed| *boxed)),
                written: Some(written),
                content: Some(Written {
                    bytes: body.as_bytes().to_vec(),
                    mode,
                }),
                kept: false,
                removal: false,
            });
            Err(committed_durability_error(&path, &source))
        }
        Err(DisplacingReplaceError::BeforeCommit(error)) => {
            if let Some(staged) = &plan.symlink {
                restore_displaced(step, staged, operation)?;
            }
            Err(before_commit_error(&path, &error, operation))
        }
        Err(DisplacingReplaceError::RecoveryRequired { quarantine, .. }) => Err(recovery_required(
            &[format!(
                "{} (original quarantined at {})",
                path.display(),
                quarantine.display()
            )],
            None,
            operation,
        )),
        Err(error) => {
            if let Some(staged) = &plan.symlink {
                restore_displaced(step, staged, operation)?;
            }
            Err(path_untrusted(
                &path,
                &format!("integration file replacement failed: {error}"),
            ))
        }
    }
}

/// Confirms a loaded input still has the content, mode, and inode it was read
/// with; anything else, including an unreadable replacement, is a collision.
fn verify_unchanged(step: &Step<'_>, operation: Operation) -> Result<Outcome, ProtocolError> {
    let path = step.dir.path.join(step.name);
    let Expected::Loaded(before) = step.expected else {
        return Ok(Outcome::Verified);
    };
    let unchanged = match (step.dir.read_optional(step.name, step.label), before) {
        (Ok(None), None) => true,
        (Ok(Some(now)), Some(before)) => {
            now.content == before.content
                && now.mode == before.mode
                && now.identity == before.identity
        }
        _ => false,
    };
    if unchanged {
        Ok(Outcome::Verified)
    } else {
        Err(before_commit_error(
            &path,
            &FsError::IdentityChanged { path: path.clone() },
            operation,
        ))
    }
}

/// Deletes an installer-owned regular file, or reports why it was left alone.
///
/// The inode is moved aside first and the ownership marker is read from that
/// very inode under its quarantine name, so a file swapped in after any earlier
/// look can never be judged by another file's content. A file that is not
/// verified as owned is moved back without replacing anything.
fn remove_owned<'a>(
    step: &'a Step<'a>,
    marker: &str,
    applied: &mut Vec<Applied<'a>>,
    operation: Operation,
) -> Result<Outcome, ProtocolError> {
    let path = step.dir.path.join(step.name);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Outcome::Absent),
        Err(error) => return Err(io_error("inspect integration destination", &path, &error)),
    };
    if !metadata.is_file() {
        return Ok(Outcome::Foreign);
    }
    race_point("remove_owned.inspected", step.name);
    let identity = match step
        .dir
        .trusted
        .entry_identity(step.name, EntryKind::RegularFile)
    {
        Ok(Some(identity)) => identity,
        Ok(None) => return Ok(Outcome::Absent),
        Err(_error) => return Ok(Outcome::Foreign),
    };
    race_point("remove_owned.identified", step.name);
    let staged = match quarantine::stage(
        &step.dir.trusted,
        step.name,
        ROLLBACK_QUARANTINE_PREFIX,
        identity,
    )
    .map_err(|error| staging_error(&error, &path, step.label, operation))?
    {
        Staging::Moved(staged) => staged,
        Staging::Missing => return Ok(Outcome::Absent),
        Staging::Refused => {
            return Err(before_commit_error(
                &path,
                &FsError::IdentityChanged { path: path.clone() },
                operation,
            ))
        }
    };
    let quarantine = staged.path();
    let verified = quarantine
        .file_name()
        .zip(fs::symlink_metadata(&quarantine).ok())
        .and_then(|(name, metadata)| {
            let mode = metadata.permissions().mode() & UNIX_MODE_MASK;
            step.dir
                .trusted
                .read_file(name, mode, SNAPSHOT_LIMIT_BYTES)
                .ok()
        })
        .filter(|bytes| {
            String::from_utf8_lossy(bytes)
                .lines()
                .any(|line| line.trim() == marker)
        });
    let Some(_verified) = verified else {
        return match quarantine::put_back(&staged, step.name) {
            Settled::Done => Ok(Outcome::Foreign),
            Settled::Left { at, why } => Err(recovery_required(
                &[format!(
                    "{} (foreign file left at {}: {why})",
                    path.display(),
                    at.display()
                )],
                None,
                operation,
            )),
            Settled::Unconfirmed(note) => Err(recovery_required(
                &[format!(
                    "{} (the foreign file was moved back but not confirmed durable: {note})",
                    path.display()
                )],
                None,
                operation,
            )),
        };
    };
    // The verified inode stays quarantined until the whole transaction
    // commits, so a later failure moves the same inode back.
    applied.push(Applied {
        step,
        displaced: Some(staged),
        written: None,
        content: None,
        kept: false,
        removal: true,
    });
    Ok(Outcome::Removed)
}

fn before_commit_error(
    path: &std::path::Path,
    error: &FsError,
    operation: Operation,
) -> ProtocolError {
    if matches!(error, FsError::IdentityChanged { .. }) {
        return ProtocolError::new(
            ErrorClass::Runtime,
            DESTINATION_COLLISION_CODE,
            format!(
                "integration destination {} changed while the installer was running; nothing was overwritten",
                path.display()
            ),
            Some(format!(
                "repeat `pohunek integration {}` once the other writer has finished",
                operation.verb()
            )),
        );
    }
    platform_path_error(path, "integration file", error)
}

fn restore_displaced(
    step: &Step<'_>,
    staged: &StagedEntry,
    operation: Operation,
) -> Result<(), ProtocolError> {
    let path = step.dir.path.join(step.name);
    match quarantine::put_back(staged, step.name) {
        Settled::Done => Ok(()),
        Settled::Left { at, why } => Err(recovery_required(
            &[format!(
                "{} (the displaced original stays at {}: {why})",
                path.display(),
                at.display()
            )],
            None,
            operation,
        )),
        Settled::Unconfirmed(note) => Err(recovery_required(
            &[format!(
                "{} (the displaced original was moved back but not confirmed durable: {note})",
                path.display()
            )],
            None,
            operation,
        )),
    }
}

/// The error for a failed attempt to move an entry aside.
///
/// A failure that names where the entry stays is a recovery report with that
/// path; one that does not left the entry where it was, so it is an unsafe
/// path.
fn staging_error(error: &FsError, path: &Path, label: &str, operation: Operation) -> ProtocolError {
    match error.recovery_path() {
        Some(at) => recovery_required(
            &[format!(
                "{} (the entry stays at {}: {error})",
                path.display(),
                at.display()
            )],
            None,
            operation,
        ),
        None => platform_path_error(path, label, error),
    }
}

/// Moves the destination aside under a quarantine name, bound to its inode.
///
/// Fails before any mutation when the entry cannot be proven safe to move.
fn stage_displaced(
    step: &Step<'_>,
    kind: EntryKind,
    operation: Operation,
) -> Result<StagedEntry, ProtocolError> {
    let path = step.dir.path.join(step.name);
    let identity = step
        .dir
        .trusted
        .entry_identity(step.name, kind)
        .map_err(|error| platform_path_error(&path, "managed entry", &error))?
        .ok_or_else(|| path_untrusted(&path, "managed entry disappeared"))?;
    match quarantine::stage(&step.dir.trusted, step.name, DISPLACED_PREFIX, identity)
        .map_err(|error| staging_error(&error, &path, "managed entry", operation))?
    {
        Staging::Moved(staged) => Ok(staged),
        Staging::Missing => Err(path_untrusted(&path, "managed entry identity changed")),
        Staging::Refused => Err(path_untrusted(&path, "managed entry staging was refused")),
    }
}

/// Re-checks every file written so far against what was written: the inode
/// from the writing descriptor, the mode, and the complete content.
///
/// A file another writer changed is kept as their version and marked so the
/// rollback never touches it; its displaced original stays quarantined and is
/// named in the error.
fn verify_written(
    applied: &mut [Applied<'_>],
    operation: Operation,
    after: Option<&str>,
) -> Result<(), ProtocolError> {
    let mut changed = Vec::new();
    for entry in applied.iter_mut() {
        let (Some(written), Some(content)) = (entry.written, entry.content.as_ref()) else {
            continue;
        };
        let step = entry.step;
        let intact = matches!(
            step.dir.trusted.entry_identity(step.name, EntryKind::RegularFile),
            Ok(Some(found)) if found == written
        ) && step
            .dir
            .trusted
            .read_file(step.name, content.mode, content.bytes.len())
            .is_ok_and(|actual| actual == content.bytes);
        if !intact {
            entry.kept = true;
            let original = entry.displaced.as_ref().map_or_else(String::new, |staged| {
                format!(" (the original stays at {})", staged.path().display())
            });
            changed.push(format!(
                "{}{original}",
                step.dir.path.join(step.name).display()
            ));
        }
    }
    if changed.is_empty() {
        return Ok(());
    }
    Err(ProtocolError::new(
        ErrorClass::Runtime,
        DESTINATION_COLLISION_CODE,
        format!(
            "integration destination changed after this {} wrote it{}; the other version was kept and nothing further was changed: {}",
            operation.verb(),
            after.map_or_else(String::new, |code| format!(" (the operation had failed with {code})")),
            changed.join(", ")
        ),
        Some(format!(
            "review the kept file(s), then repeat `pohunek integration {}` once the other writer has finished",
            operation.verb()
        )),
    ))
}

/// Rolls back after a failed step, first re-verifying every file written so far.
///
/// A file another writer changed since it was written is kept as their version
/// and reported as a collision instead of being replaced by the original.
fn abort(
    mut applied: Vec<Applied<'_>>,
    cause: ProtocolError,
    operation: Operation,
) -> ProtocolError {
    let cause = match verify_written(&mut applied, operation, Some(&cause.code)) {
        Ok(()) => cause,
        Err(collision) => collision,
    };
    roll_back(applied, cause, operation)
}

/// Restores every applied step in reverse order.
fn roll_back(
    applied: Vec<Applied<'_>>,
    cause: ProtocolError,
    operation: Operation,
) -> ProtocolError {
    let mut unrestored = Vec::new();
    for entry in applied.into_iter().rev() {
        if let Err(reason) = restore(&entry) {
            unrestored.push(format!(
                "{} ({reason})",
                entry.step.dir.path.join(entry.step.name).display()
            ));
        }
    }
    if unrestored.is_empty() {
        cause
    } else {
        recovery_required(&unrestored, Some(&cause.code), operation)
    }
}

/// Restores one applied step; a failure says where the data really is.
fn restore(entry: &Applied<'_>) -> Result<(), String> {
    let step = entry.step;
    if entry.kept {
        return Ok(());
    }
    if !entry.removal {
        remove_written(entry).map_err(|reason| match &entry.displaced {
            Some(staged) => format!(
                "{reason}; the original stays at {}",
                staged.path().display()
            ),
            None => reason,
        })?;
    }
    match &entry.displaced {
        None => Ok(()),
        Some(staged) => match quarantine::put_back(staged, step.name) {
            Settled::Done => Ok(()),
            Settled::Left { at, why } => Err(format!(
                "the original could not be moved back and stays at {} ({why})",
                at.display()
            )),
            Settled::Unconfirmed(note) => Err(format!(
                "the original was moved back but not confirmed durable ({note})"
            )),
        },
    }
}

/// Removes the file a rolled-back step wrote, only while it is still that inode.
///
/// The recorded identity comes from the descriptor that wrote the file before
/// its activation. The current destination is moved aside bound to it: a
/// different inode is put back untouched and reported, never deleted, and any
/// outcome that leaves the file in quarantine names where it is.
fn remove_written(entry: &Applied<'_>) -> Result<(), String> {
    let step = entry.step;
    let identity = entry
        .written
        .ok_or_else(|| "written file identity is unknown".to_owned())?;
    let staged = match quarantine::stage(
        &step.dir.trusted,
        step.name,
        ROLLBACK_QUARANTINE_PREFIX,
        identity,
    ) {
        Ok(Staging::Moved(staged)) => staged,
        Ok(Staging::Missing) => return Ok(()),
        Ok(Staging::Refused) => return Err("written file changed before rollback".to_owned()),
        Err(error) => {
            return Err(error.recovery_path().map_or_else(
                || format!("written file could not be moved aside ({error})"),
                |at| {
                    format!(
                        "written file could not be moved aside and stays at {} ({error})",
                        at.display()
                    )
                },
            ));
        }
    };
    // Judge the moved inode itself: a change made since the file was written is
    // kept, never removed.
    let intact = entry.content.as_ref().is_none_or(|content| {
        staged.path().file_name().is_some_and(|name| {
            step.dir
                .trusted
                .read_file(name, content.mode, content.bytes.len())
                .is_ok_and(|actual| actual == content.bytes)
        })
    });
    if !intact {
        return Err(match quarantine::put_back(&staged, step.name) {
            Settled::Done => {
                "the file was changed after it was written; that version is kept".to_owned()
            }
            Settled::Left { at, why } => format!(
                "the file was changed after it was written and could not be put back; it stays at {} ({why})",
                at.display()
            ),
            Settled::Unconfirmed(note) => format!(
                "the file was changed after it was written; it was put back but not confirmed durable ({note})"
            ),
        });
    }
    race_point("rollback.verified", step.name);
    match quarantine::discard(staged) {
        Settled::Done | Settled::Unconfirmed(_) => Ok(()),
        Settled::Left { at, why } => Err(format!(
            "the written file could not be removed and stays at {} ({why})",
            at.display()
        )),
    }
}

pub(super) fn recovery_required(
    unrestored: &[String],
    cause_code: Option<&str>,
    operation: Operation,
) -> ProtocolError {
    let cause = cause_code.map_or_else(String::new, |code| format!(" after {code}"));
    let verb = operation.verb();
    ProtocolError::new(
        ErrorClass::Runtime,
        RECOVERY_REQUIRED_CODE,
        format!(
            "integration {verb} failed{cause} and rollback could not restore: {}",
            unrestored.join(", ")
        ),
        Some(format!(
            "inspect the listed files, repair them by hand, then rerun `pohunek integration {verb}`"
        )),
    )
}
