//! Deletion and startup collection of durable worker journal directories.
//!
//! A directory `workers/<session>` holds the journals of every worker a
//! session ever had. It is evidence while the session exists, and orphaned
//! weight once the session and its workers are gone. Two paths delete it:
//! removal of a session deletes it after the session record, and daemon
//! startup collects the directories no startup evidence names.
//!
//! A directory is deleted by renaming it to a tombstone (`.gc.<session>...`),
//! so a reader never sees a half-deleted session, and then unlinking only the
//! names a journal directory can hold. A name outside that grammar keeps the
//! tombstone in place and is reported.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use pohunek_platform::filesystem::{EmptyRemoval, FsResult, MoveOutcome, TrustedDir};
use pohunek_platform::process::ProcessIdentity;
use pohunek_session_worker::RuntimePhase;
use tracing::{info, warn};

use super::reconcile::{open_journal_root, JournalReject, WorkerJournalScan};
use super::tmpdir::TmpdirOwners;
use super::SessionRegistry;
use protocol::SessionId;

/// Mode of the owner-private worker journal directories.
const JOURNAL_DIR_MODE: u32 = 0o700;

/// Distinguishes the tombstones one daemon process makes.
static TOMBSTONE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// What deleting journal directories freed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Reclaimed {
    /// Journal and temporary files unlinked.
    pub(super) files: u64,
    /// Bytes those files held.
    pub(super) bytes: u64,
}

impl Reclaimed {
    fn add(&mut self, other: Self) {
        self.files += other.files;
        self.bytes += other.bytes;
    }
}

/// The outcome of deleting one journal directory.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Deletion {
    /// The directory and its tombstone are gone.
    Deleted(Reclaimed),
    /// The tombstone still holds a name outside the journal grammar.
    Kept {
        /// Name of the remaining tombstone.
        tombstone: String,
        /// What was unlinked before the unknown name stopped the removal.
        reclaimed: Reclaimed,
    },
    /// There was no directory.
    Missing,
}

/// Whether `name` is a worker journal file.
fn is_journal_file(name: &str) -> bool {
    name.strip_suffix(".json")
        .is_some_and(|stem| pohunek_paths::valid_worker_id(stem).is_some())
}

/// Whether `name` is a file a journal directory may hold.
fn is_journal_directory_name(name: &str) -> bool {
    is_journal_file(name) || pohunek_paths::parse_journal_temp_name(name).is_some()
}

/// Deletes the journal directory of `session_id` below `state_root`.
///
/// # Errors
///
/// Returns the I/O error when the worker state root is unsafe or a delete step
/// fails; the directory is then left as a tombstone or in place, and startup
/// collection finishes it.
pub(super) fn remove_session_journals(
    state_root: &Path,
    session_id: &str,
) -> std::io::Result<Deletion> {
    let Some(root) = open_journal_root(state_root)? else {
        return Ok(Deletion::Missing);
    };
    if pohunek_paths::valid_worker_session_id(session_id).is_none() {
        return Ok(Deletion::Missing);
    }
    delete_session_directory(&root, state_root, session_id).map_err(std::io::Error::other)
}

/// Renames the directory `session_id` to a tombstone and removes the tombstone.
fn delete_session_directory(
    root: &TrustedDir,
    state_root: &Path,
    session_id: &str,
) -> FsResult<Deletion> {
    let sequence = TOMBSTONE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let tombstone = pohunek_paths::worker_tombstone_name(session_id, std::process::id(), sequence);
    match root.move_no_replace_unsynced(session_id, root, &tombstone) {
        Ok(MoveOutcome::Moved) => {}
        Ok(MoveOutcome::DestinationExists | _) => return Ok(Deletion::Missing),
        Err(error) if error.io_kind() == Some(std::io::ErrorKind::NotFound) => {
            return Ok(Deletion::Missing);
        }
        Err(error) => return Err(error),
    }
    finish_tombstone(root, state_root, &tombstone)
}

/// Unlinks the journal and temporary files of `tombstone`, then removes it.
fn finish_tombstone(root: &TrustedDir, state_root: &Path, tombstone: &str) -> FsResult<Deletion> {
    let directory = root.open_child(tombstone, JOURNAL_DIR_MODE)?;
    let mut reclaimed = Reclaimed::default();
    for name in directory.entry_names()? {
        let Some(text) = name.to_str().filter(|text| is_journal_directory_name(text)) else {
            continue;
        };
        let size = std::fs::symlink_metadata(state_root.join(tombstone).join(text))
            .map_or(0, |metadata| metadata.len());
        if directory.remove_entry(&name)? {
            reclaimed.add(Reclaimed {
                files: 1,
                bytes: size,
            });
        }
    }
    Ok(match root.remove_empty_child(tombstone)? {
        EmptyRemoval::Removed | EmptyRemoval::Missing => Deletion::Deleted(reclaimed),
        _ => Deletion::Kept {
            tombstone: tombstone.to_owned(),
            reclaimed,
        },
    })
}

/// What one startup collection pass decided, for the single summary line.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct CollectionSummary {
    /// Journal directories deleted.
    pub(super) deleted_directories: u64,
    /// Files and bytes those deletions and the temporary-file sweep freed.
    pub(super) reclaimed: Reclaimed,
    /// Leftover unfinished journal writes removed.
    pub(super) temporary_files: u64,
    /// Leftover tombstones finished.
    pub(super) tombstones: u64,
    /// Tombstones that still hold an unknown name.
    pub(super) tombstones_kept: u64,
    /// Directories kept because a session, resume binding, socket, claim or
    /// supervisor job names them.
    pub(super) kept_owned: u64,
    /// Directories kept because a journaled worker may still run.
    pub(super) kept_live: u64,
    /// Directories kept because something in them changed within the window.
    pub(super) kept_in_window: u64,
    /// Directories kept because a file is unreadable, mismatched, unknown, or
    /// states no worker identity.
    pub(super) kept_unreadable: u64,
    /// Directories kept because a journal declares a schema newer than this
    /// build reads.
    pub(super) kept_newer_schema: u64,
    /// Directories whose deletion failed and is retried at the next start.
    pub(super) failed: u64,
}

/// The evidence one collection pass judges directories against.
pub(super) struct Collection<'a> {
    /// The worker state root, `workers/`.
    pub(super) state_root: &'a Path,
    /// Every session a directory may belong to; any other directory is a
    /// candidate.
    pub(super) owners: &'a HashSet<String>,
    /// The journals of the startup scan, by session.
    pub(super) scans: &'a HashMap<String, WorkerJournalScan>,
    /// One snapshot of the same-user process table taken for this pass.
    pub(super) processes: &'a HashSet<ProcessIdentity>,
    /// Age after which an untouched directory of a record-less session is
    /// collected, and after which an unfinished write is stale even when its
    /// process id is in use.
    pub(super) max_age: Duration,
    /// The time ages are measured against.
    pub(super) now: SystemTime,
}

/// Why a candidate directory was kept.
enum Kept {
    Live,
    InWindow,
    Unreadable,
    NewerSchema,
}

impl Collection<'_> {
    /// Collects the journal directories of sessions that no longer exist.
    ///
    /// # Errors
    ///
    /// Returns the I/O error when the worker state root is unsafe or cannot be
    /// listed; nothing is deleted then.
    pub(super) fn run(&self) -> std::io::Result<CollectionSummary> {
        let mut summary = CollectionSummary::default();
        let Some(root) = open_journal_root(self.state_root)? else {
            return Ok(summary);
        };
        let pids: HashSet<u32> = self.processes.iter().map(|process| process.pid).collect();
        for name in root.entry_names().map_err(std::io::Error::other)? {
            let Some(text) = name.to_str() else {
                continue;
            };
            if pohunek_paths::parse_worker_tombstone(text).is_some() {
                match finish_tombstone(&root, self.state_root, text) {
                    Ok(Deletion::Deleted(reclaimed)) => {
                        summary.tombstones += 1;
                        summary.reclaimed.add(reclaimed);
                    }
                    Ok(Deletion::Kept { reclaimed, .. }) => {
                        summary.tombstones_kept += 1;
                        summary.reclaimed.add(reclaimed);
                    }
                    Ok(Deletion::Missing) => {}
                    Err(_unreadable) => summary.failed += 1,
                }
                continue;
            }
            if pohunek_paths::valid_worker_session_id(text).is_none() {
                continue;
            }
            self.visit_directory(&root, text, &pids, &mut summary);
        }
        Ok(summary)
    }

    /// Sweeps stale unfinished writes of one session directory and deletes the
    /// directory when it is an orphan whose workers are provably gone.
    fn visit_directory(
        &self,
        root: &TrustedDir,
        session_id: &str,
        pids: &HashSet<u32>,
        summary: &mut CollectionSummary,
    ) {
        let Ok(directory) = root.open_child(session_id, JOURNAL_DIR_MODE) else {
            summary.kept_unreadable += 1;
            return;
        };
        let Ok(entries) = directory.entry_names() else {
            summary.kept_unreadable += 1;
            return;
        };
        let path = self.state_root.join(session_id);
        let newest = newest_change(&path, &entries);
        let mut remaining = Vec::with_capacity(entries.len());
        for entry in entries {
            let Some(name) = entry.to_str() else {
                remaining.push(entry);
                continue;
            };
            let Some((_, pid)) = pohunek_paths::parse_journal_temp_name(name) else {
                remaining.push(entry);
                continue;
            };
            let metadata = std::fs::symlink_metadata(path.join(name));
            let stale = !pids.contains(&pid) || metadata.as_ref().is_ok_and(|m| self.is_old(m));
            if !stale {
                remaining.push(entry);
                continue;
            }
            let size = metadata.map_or(0, |metadata| metadata.len());
            match directory.remove_entry(&entry) {
                Ok(true) => {
                    summary.temporary_files += 1;
                    summary.reclaimed.add(Reclaimed {
                        files: 1,
                        bytes: size,
                    });
                }
                Ok(false) => {}
                Err(_failure) => remaining.push(entry),
            }
        }
        if self.owners.contains(session_id) {
            summary.kept_owned += 1;
            return;
        }
        match self.judge(session_id, &remaining, newest) {
            Err(Kept::Live) => summary.kept_live += 1,
            Err(Kept::InWindow) => summary.kept_in_window += 1,
            Err(Kept::Unreadable) => summary.kept_unreadable += 1,
            Err(Kept::NewerSchema) => summary.kept_newer_schema += 1,
            Ok(()) => match delete_session_directory(root, self.state_root, session_id) {
                Ok(Deletion::Deleted(reclaimed)) => {
                    summary.deleted_directories += 1;
                    summary.reclaimed.add(reclaimed);
                }
                Ok(Deletion::Kept { reclaimed, .. }) => {
                    summary.tombstones_kept += 1;
                    summary.reclaimed.add(reclaimed);
                }
                Ok(Deletion::Missing) => {}
                Err(_failure) => summary.failed += 1,
            },
        }
    }

    /// Whether `metadata` names a change older than the window.
    fn is_old(&self, metadata: &std::fs::Metadata) -> bool {
        metadata
            .modified()
            .ok()
            .and_then(|modified| self.now.duration_since(modified).ok())
            .is_some_and(|age| age >= self.max_age)
    }

    /// Decides whether the directory of an unowned session may go.
    ///
    /// `Ok` only when it is untouched for the window, holds nothing but the
    /// journal grammar, every journal is understood, and every worker they
    /// name is gone from the process snapshot.
    fn judge(
        &self,
        session_id: &str,
        remaining: &[std::ffi::OsString],
        newest: Option<SystemTime>,
    ) -> Result<(), Kept> {
        let Some(newest) = newest else {
            return Err(Kept::Unreadable);
        };
        let aged = self
            .now
            .duration_since(newest)
            .is_ok_and(|age| age >= self.max_age);
        if !aged {
            return Err(Kept::InWindow);
        }
        let mut journal_files = 0_usize;
        for entry in remaining {
            match entry.to_str() {
                Some(name) if is_journal_file(name) => journal_files += 1,
                _ => return Err(Kept::Unreadable),
            }
        }
        let Some(scan) = self.scans.get(session_id) else {
            return if journal_files == 0 {
                Ok(())
            } else {
                Err(Kept::Unreadable)
            };
        };
        let accounted = scan.evidence.len()
            + scan
                .rejected
                .iter()
                .filter(|(name, _)| !name.is_empty())
                .count();
        if accounted != journal_files || scan.rejected.iter().any(|(name, _)| name.is_empty()) {
            return Err(Kept::Unreadable);
        }
        for evidence in &scan.evidence {
            let identity = evidence
                .worker()
                .identity()
                .map_err(|_unreadable| Kept::Unreadable)?;
            if self.processes.contains(&identity) {
                return Err(Kept::Live);
            }
        }
        for (name, reject) in &scan.rejected {
            match reject {
                JournalReject::UnsupportedSchema(_) => {
                    let outdated = scan
                        .outdated
                        .iter()
                        .find(|(known, _)| known == name)
                        .map(|(_, outdated)| outdated)
                        .ok_or(Kept::NewerSchema)?;
                    if outdated.phase.as_ref().is_some_and(is_final) {
                        continue;
                    }
                    let identity = outdated
                        .worker_pid
                        .zip(outdated.worker_start_identity.as_deref())
                        .and_then(|(pid, start)| {
                            start.parse().ok().map(|start_identity| ProcessIdentity {
                                pid,
                                start_identity,
                            })
                        })
                        .ok_or(Kept::Unreadable)?;
                    if self.processes.contains(&identity) {
                        return Err(Kept::Live);
                    }
                }
                JournalReject::Unreadable | JournalReject::Mismatched => {
                    return Err(Kept::Unreadable);
                }
            }
        }
        Ok(())
    }
}

/// Whether a worker in `phase` no longer owns a PTY child.
fn is_final(phase: &RuntimePhase) -> bool {
    matches!(
        phase,
        RuntimePhase::Terminal | RuntimePhase::NeverInitialized | RuntimePhase::Faulted
    )
}

/// The newest modification time of the directory `path` and its `entries`;
/// `None` when any of them cannot be read. A future time counts as is, so a
/// clock step back keeps the directory.
fn newest_change(path: &Path, entries: &[std::ffi::OsString]) -> Option<SystemTime> {
    let mut newest = std::fs::symlink_metadata(path).ok()?.modified().ok()?;
    for entry in entries {
        let modified = std::fs::symlink_metadata(path.join(entry))
            .ok()?
            .modified()
            .ok()?;
        newest = newest.max(modified);
    }
    Some(newest)
}

impl SessionRegistry {
    /// Deletes the journal directory of the removed session `id`.
    ///
    /// Runs after the session record is gone: until then a replayed removal
    /// still needs the journals to prove its workers gone. A failure is logged
    /// and left to the startup collection, because the removal already
    /// succeeded.
    pub(super) async fn delete_session_journals(&self, id: &SessionId) {
        let Some(state_root) = self.inner.config.worker_state_root.clone() else {
            return;
        };
        let session_id = id.0.clone();
        let outcome =
            tokio::task::spawn_blocking(move || remove_session_journals(&state_root, &session_id))
                .await;
        match outcome {
            Ok(Ok(Deletion::Kept { tombstone, .. })) => warn!(
                session_id = %id.0,
                tombstone = %tombstone,
                "a removed session's journal directory holds an unknown name and was kept"
            ),
            Ok(Ok(Deletion::Deleted(_) | Deletion::Missing)) => {}
            Ok(Err(error)) => warn!(
                session_id = %id.0,
                error = %error,
                "deleting the journals of a removed session failed; startup collects them"
            ),
            Err(error) => warn!(
                session_id = %id.0,
                error = %error,
                "deleting the journals of a removed session panicked"
            ),
        }
    }

    /// Collects the journal directories of sessions that no longer exist.
    ///
    /// Runs once at startup, after the temporary-directory sweep and before
    /// the control server accepts requests. It is skipped as a whole when the
    /// owners cannot be established or the process table cannot be read.
    pub(super) async fn collect_worker_journals(
        &self,
        owners: TmpdirOwners,
        scans: HashMap<String, WorkerJournalScan>,
    ) {
        let Some(state_root) = self.inner.config.worker_state_root.clone() else {
            return;
        };
        let owners = match owners {
            TmpdirOwners::Known(owners) => owners,
            TmpdirOwners::Unknown { reason, .. } => {
                info!(
                    reason = %reason,
                    "orphaned worker journals are kept: their sessions cannot be established"
                );
                return;
            }
        };
        let processes = match self.inner.inspector.same_user_processes() {
            Ok(facts) => facts
                .iter()
                .map(pohunek_platform::process::ProcessFact::identity)
                .collect::<HashSet<_>>(),
            Err(error) => {
                info!(
                    error = %error,
                    "orphaned worker journals are kept: the process table cannot be read"
                );
                return;
            }
        };
        let max_age = self.inner.config.worker_journal_max_age;
        let outcome = tokio::task::spawn_blocking(move || {
            Collection {
                state_root: &state_root,
                owners: &owners,
                scans: &scans,
                processes: &processes,
                max_age,
                now: SystemTime::now(),
            }
            .run()
        })
        .await;
        match outcome {
            Ok(Ok(summary)) => info!(
                deleted_directories = summary.deleted_directories,
                files = summary.reclaimed.files,
                bytes = summary.reclaimed.bytes,
                temporary_files = summary.temporary_files,
                tombstones = summary.tombstones,
                tombstones_kept = summary.tombstones_kept,
                kept_owned = summary.kept_owned,
                kept_live = summary.kept_live,
                kept_in_window = summary.kept_in_window,
                kept_unreadable = summary.kept_unreadable,
                kept_newer_schema = summary.kept_newer_schema,
                failed = summary.failed,
                "worker journal collection finished"
            ),
            Ok(Err(error)) => warn!(
                error = %error,
                "worker journal collection failed; the directories are kept"
            ),
            Err(error) => warn!(error = %error, "worker journal collection panicked"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    use pohunek_platform::process::{ProcessIdentity, StartIdentity};
    use pohunek_session_worker::{Journal, JournalRecord, RuntimePhase, WorkerOrigin};

    use super::{Collection, CollectionSummary};
    use crate::session::reconcile::scan_worker_journals;

    /// Age after which an untouched directory is collected.
    const WINDOW: Duration = Duration::from_hours(168);

    /// Age of a directory that left the window.
    const AGED: Duration = Duration::from_hours(720);

    /// Process id and start identity of the worker a fixture journal names.
    const WORKER: (u32, u64) = (41, 410);

    fn private_dir(path: &Path) {
        std::fs::create_dir_all(path).expect("create the directory");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("secure the directory");
    }

    /// Writes a current-schema journal `worker` of `session` below `dir` for a
    /// worker with process id `pid`.
    fn write_journal(dir: &Path, session: &str, worker: &str, pid: u32) {
        write_journal_started(dir, session, worker, (pid, WORKER.1));
    }

    /// [`write_journal`] for a worker with the process identity `(pid, start)`.
    fn write_journal_started(dir: &Path, session: &str, worker: &str, (pid, start): (u32, u64)) {
        let mut journal = JournalRecord::bootstrap(
            session.to_owned(),
            worker.to_owned(),
            WorkerOrigin {
                executable: PathBuf::from("/usr/libexec/pohunek-sessiond"),
                version: "0.0.0-test".to_owned(),
                generation: "abcd2345".to_owned(),
            },
            pid,
            start.to_string(),
            "boot-test".to_owned(),
            (1, 2),
            "2026-09-20T00:00:00Z".to_owned(),
        );
        journal.phase = RuntimePhase::Terminal;
        Journal::new(dir.join(format!("{worker}.json")))
            .write(&journal)
            .expect("write the journal");
    }

    fn write_file(dir: &Path, name: &str, contents: &[u8]) {
        let path = dir.join(name);
        std::fs::write(&path, contents).expect("write the file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("secure the file");
    }

    /// Sets the modification time of `path` to `age` before now.
    fn age(path: &Path, age: Duration) {
        std::fs::File::open(path)
            .expect("open for aging")
            .set_modified(SystemTime::now() - age)
            .expect("set the modification time");
    }

    /// Ages the directory and everything in it.
    fn age_tree(dir: &Path, by: Duration) {
        for entry in std::fs::read_dir(dir).expect("read the directory") {
            age(&entry.expect("entry").path(), by);
        }
        age(dir, by);
    }

    /// An aged session directory holding one journal of a worker with `pid`.
    fn aged_session(root: &Path, session: &str, pid: u32) -> PathBuf {
        let dir = root.join(session);
        private_dir(&dir);
        write_journal(&dir, session, "worker-aaaa", pid);
        age_tree(&dir, AGED);
        dir
    }

    fn live_process(pid: u32) -> ProcessIdentity {
        ProcessIdentity {
            pid,
            start_identity: StartIdentity::new(WORKER.1),
        }
    }

    fn collect(root: &Path, owners: &[&str], processes: &[ProcessIdentity]) -> CollectionSummary {
        let scans = scan_worker_journals(root).expect("scan the journals");
        let owners: HashSet<String> = owners.iter().map(|id| (*id).to_owned()).collect();
        let processes: HashSet<ProcessIdentity> = processes.iter().copied().collect();
        Collection {
            state_root: root,
            owners: &owners,
            scans: &scans,
            processes: &processes,
            max_age: WINDOW,
            now: SystemTime::now(),
        }
        .run()
        .expect("collect")
    }

    #[test]
    fn the_collection_deletes_only_aged_orphans_whose_workers_are_gone() {
        let temp = pohunek_test_support::tempdir().expect("fixture directory");
        let root = temp.path().join("workers");
        private_dir(&root);

        // Deleted: aged, no owner, the worker is not in the process snapshot.
        let dead = aged_session(&root, "s-1", 41);
        // Kept: the worker the journal names is running.
        let live = aged_session(&root, "s-2", 42);
        // Kept: changed within the window.
        let fresh = root.join("s-3");
        private_dir(&fresh);
        write_journal(&fresh, "s-3", "worker-aaaa", 41);
        // Kept: a record, resume binding, claim or job names the session.
        let owned = aged_session(&root, "s-4", 41);
        // Kept: a journal of a newer schema than this build reads.
        let newer = root.join("s-5");
        private_dir(&newer);
        write_file(
            &newer,
            "worker-aaaa.json",
            br#"{"schema_version":99,"session_id":"s-5"}"#,
        );
        age_tree(&newer, AGED);
        // Deleted: an older schema whose worker identity is gone.
        let old_dead = root.join("s-6");
        private_dir(&old_dead);
        write_file(
            &old_dead,
            "worker-aaaa.json",
            br#"{"schema_version":1,"session_id":"s-6","worker_pid":43,"worker_start_identity":"410","phase":"live"}"#,
        );
        age_tree(&old_dead, AGED);
        // Kept: an older schema whose worker identity still runs.
        let old_live = root.join("s-7");
        private_dir(&old_live);
        write_file(
            &old_live,
            "worker-aaaa.json",
            br#"{"schema_version":1,"session_id":"s-7","worker_pid":44,"worker_start_identity":"410","phase":"live"}"#,
        );
        age_tree(&old_live, AGED);
        // Kept: a file that cannot be decoded.
        let unreadable = root.join("s-8");
        private_dir(&unreadable);
        write_file(&unreadable, "worker-aaaa.json", b"not json");
        age_tree(&unreadable, AGED);
        // Kept: a name outside the journal grammar.
        let unknown = aged_session(&root, "s-9", 41);
        write_file(&unknown, "notes.txt", b"keep me");
        age_tree(&unknown, AGED);
        // Kept: a modification time in the future.
        let future = aged_session(&root, "s-10", 41);
        age(&future, Duration::ZERO);
        std::fs::File::open(&future)
            .expect("open")
            .set_modified(SystemTime::now() + Duration::from_hours(24))
            .expect("set a future time");
        // Finished: a leftover tombstone.
        let tombstone = root.join(".gc.s-11.100.1");
        private_dir(&tombstone);
        write_file(&tombstone, "worker-aaaa.json", b"{}");
        // Swept: stale unfinished writes inside directories that stay.
        let stale_temp = ".worker-aaaa.json.999999.1.tmp";
        write_file(&owned, stale_temp, b"{}");
        let live_temp = format!(".worker-aaaa.json.{}.2.tmp", 42);
        write_file(&owned, &live_temp, b"{}");

        let summary = collect(&root, &["s-4"], &[live_process(42), live_process(44)]);

        assert!(!dead.exists(), "an aged orphan with a dead worker goes");
        assert!(
            !old_dead.exists(),
            "an older-schema orphan with a dead worker goes"
        );
        assert!(!tombstone.exists(), "a leftover tombstone is finished");
        for kept in [
            &live,
            &fresh,
            &owned,
            &newer,
            &old_live,
            &unreadable,
            &unknown,
            &future,
        ] {
            assert!(kept.is_dir(), "{} is kept", kept.display());
        }
        assert!(
            owned.join("worker-aaaa.json").exists() && !owned.join(stale_temp).exists(),
            "a dead writer's temporary file goes and the journal stays"
        );
        assert!(
            owned.join(&live_temp).exists(),
            "a live writer's temporary file stays"
        );
        assert_eq!(
            summary,
            CollectionSummary {
                deleted_directories: 2,
                reclaimed: summary.reclaimed,
                temporary_files: 1,
                tombstones: 1,
                tombstones_kept: 0,
                kept_owned: 1,
                kept_live: 2,
                kept_in_window: 2,
                kept_unreadable: 2,
                kept_newer_schema: 1,
                failed: 0,
            }
        );
        assert_eq!(summary.reclaimed.files, 4);
        // Nothing else is left to collect on the next start.
        let again = collect(&root, &["s-4"], &[live_process(42), live_process(44)]);
        assert_eq!(again.deleted_directories, 0);
        assert_eq!(again.tombstones, 0);
    }

    /// The identity a worker journals matches what the process table reports,
    /// so a record-less directory of a running worker is kept.
    #[test]
    fn a_running_worker_of_the_real_process_table_keeps_its_directory() {
        use crate::procwatch::ProcessInspector as _;
        let host = crate::procwatch::readable_host::ReadableHost::new();
        let me = host
            .identity(std::process::id())
            .expect("inspect this process")
            .expect("this process runs");
        let temp = pohunek_test_support::tempdir().expect("fixture directory");
        let root = temp.path().join("workers");
        private_dir(&root);
        let directory = root.join("s-1");
        private_dir(&directory);
        write_journal_started(
            &directory,
            "s-1",
            "worker-aaaa",
            (
                me.pid,
                me.start_identity.to_string().parse().expect("number"),
            ),
        );
        age_tree(&directory, AGED);
        let processes: Vec<ProcessIdentity> = host
            .same_user_processes()
            .expect("read the process table")
            .iter()
            .map(pohunek_platform::process::ProcessFact::identity)
            .collect();

        let summary = collect(&root, &[], &processes);

        assert_eq!(summary.kept_live, 1);
        assert!(directory.is_dir());
    }

    #[test]
    fn a_tombstone_with_an_unknown_name_is_kept_and_reported() {
        let temp = pohunek_test_support::tempdir().expect("fixture directory");
        let root = temp.path().join("workers");
        private_dir(&root);
        let tombstone = root.join(".gc.s-1.100.1");
        private_dir(&tombstone);
        write_file(&tombstone, "worker-aaaa.json", b"{}");
        write_file(&tombstone, "notes.txt", b"keep me");

        let summary = collect(&root, &[], &[]);

        assert_eq!(summary.tombstones_kept, 1);
        assert!(tombstone.join("notes.txt").exists());
        assert!(!tombstone.join("worker-aaaa.json").exists());
    }

    #[test]
    fn a_missing_state_root_collects_nothing() {
        let temp = pohunek_test_support::tempdir().expect("fixture directory");
        let summary = collect(&temp.path().join("workers"), &[], &[]);
        assert_eq!(summary, CollectionSummary::default());
    }
}
