//! Real-filesystem tests for live transcript watching.
//!
//! The tests drive the platform backend (inotify on Linux, `FSEvents` on macOS)
//! through `start_transcript_index` and assert on the resulting index and
//! watcher health. They wait on the observer's wake-up signal and on health
//! changes rather than sleeping, and contain no platform-specific expectations
//! beyond the documented per-backend degradation cause.

// Rust guideline compliant 2026-09-29

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use protocol::RuntimeRef;

use super::watch::{TranscriptWatch, WatcherDegraded};
use super::{
    start_transcript_index, ExternalSessions, TranscriptIndex, TranscriptRoot, WatcherHandle,
};

/// Upper bound on waiting for a filesystem change to reach the index.
///
/// Generous because `FSEvents` delivers events on its own thread with kernel
/// coalescing latency; a healthy run finishes in milliseconds.
const LIVE_TEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Interval at which the tests tick the retry like the observer's sweep.
const SWEEP_TICK: Duration = Duration::from_millis(200);
/// Directory mode that denies every access.
const DENIED_DIR_MODE: u32 = 0o000;
/// File mode that only the owner may write.
const WRITE_ONLY_FILE_MODE: u32 = 0o200;
/// File mode restored to make a transcript readable.
const READABLE_FILE_MODE: u32 = 0o644;
/// Directory mode restored so fixtures can be deleted.
const RESTORED_DIR_MODE: u32 = 0o755;

struct Live {
    base: tempfile::TempDir,
    root: PathBuf,
    sessions: ExternalSessions,
    index: TranscriptIndex,
    handle: WatcherHandle,
}

fn record(session_id: &str) -> String {
    format!("{{\"session_id\":\"{session_id}\",\"cwd\":\"/work\"}}\n")
}

fn write_transcript(path: &Path, session_id: &str) {
    fs::create_dir_all(path.parent().expect("transcript parent")).expect("create parent");
    fs::write(path, record(session_id)).expect("write transcript");
}

impl Live {
    /// Starts watching `<base>/projects`, which the caller may create first.
    async fn start(prepare: impl FnOnce(&Path, &Path)) -> Self {
        let base = pohunek_test_support::tempdir().expect("fixture base");
        let root = base.path().join("projects");
        prepare(base.path(), &root);
        let sessions = ExternalSessions::new();
        let roots = vec![TranscriptRoot {
            agent_base: RuntimeRef::claude(),
            path: root.clone(),
        }];
        let (index, handle) = start_transcript_index(&roots, &sessions).await;
        Self {
            base,
            root,
            sessions,
            index,
            handle,
        }
    }

    fn session_id(&self, path: &Path) -> Option<String> {
        self.index
            .inner
            .lock()
            .expect("index")
            .get(path)
            .and_then(|candidate| candidate.native_session_id.clone())
    }

    fn is_indexed(&self, path: &Path) -> bool {
        self.index.inner.lock().expect("index").contains_key(path)
    }

    async fn wait_until(&self, what: &str, done: impl Fn(&Self) -> bool) {
        let wait = async {
            while !done(self) {
                self.sessions.inner.rescan.notified().await;
            }
        };
        tokio::time::timeout(LIVE_TEST_TIMEOUT, wait)
            .await
            .unwrap_or_else(|_elapsed| panic!("timed out waiting for {what}"));
    }

    async fn wait_for_session(&self, path: &Path, session_id: &str) {
        self.wait_until(&format!("{session_id} at {}", path.display()), |live| {
            live.session_id(path).as_deref() == Some(session_id)
        })
        .await;
    }

    async fn wait_until_gone(&self, path: &Path) {
        self.wait_until(&format!("{} to leave the index", path.display()), |live| {
            !live.is_indexed(path)
        })
        .await;
    }

    /// Waits for a watcher state while ticking the retry the way the observer's
    /// sweep does, since registration retries are backed off between ticks.
    async fn wait_for_state(&self, what: &str, done: impl Fn(TranscriptWatch) -> bool) {
        let mut states = self.handle.subscribe();
        let wait = async {
            loop {
                self.handle.request_reconcile();
                if tokio::time::timeout(SWEEP_TICK, states.wait_for(|state| done(*state)))
                    .await
                    .is_ok()
                {
                    return;
                }
            }
        };
        tokio::time::timeout(LIVE_TEST_TIMEOUT, wait)
            .await
            .unwrap_or_else(|_elapsed| panic!("timed out waiting for watcher state {what}"));
    }
}

#[tokio::test]
async fn a_healthy_root_reports_active_and_indexes_existing_transcripts() {
    let live =
        Live::start(|_base, root| write_transcript(&root.join("p/s.jsonl"), "existing")).await;

    assert_eq!(live.handle.state(), TranscriptWatch::Active);
    assert_eq!(
        live.session_id(&live.root.join("p/s.jsonl")).as_deref(),
        Some("existing")
    );
}

#[tokio::test]
async fn creating_a_transcript_indexes_it() {
    let live =
        Live::start(|_base, root| fs::create_dir_all(root.join("p")).expect("project")).await;
    let path = live.root.join("p/new.jsonl");

    write_transcript(&path, "created");

    live.wait_for_session(&path, "created").await;
}

#[tokio::test]
async fn appending_the_identifying_record_indexes_a_transcript() {
    let live = Live::start(|_base, root| {
        fs::create_dir_all(root.join("p")).expect("project");
        fs::write(root.join("p/s.jsonl"), "{\"type\":\"noise\"}\n").expect("noise");
    })
    .await;
    let path = live.root.join("p/s.jsonl");
    assert!(!live.is_indexed(&path));

    let mut file = OpenOptions::new().append(true).open(&path).expect("open");
    file.write_all(record("appended").as_bytes())
        .expect("append");
    drop(file);

    live.wait_for_session(&path, "appended").await;
}

#[tokio::test]
async fn atomic_replacement_updates_the_candidate() {
    let live = Live::start(|_base, root| write_transcript(&root.join("p/s.jsonl"), "old")).await;
    let path = live.root.join("p/s.jsonl");
    let staged = live.root.join("p/s.tmp");

    fs::write(&staged, record("replacement")).expect("stage");
    fs::rename(&staged, &path).expect("rename over");

    live.wait_for_session(&path, "replacement").await;
}

#[tokio::test]
async fn truncation_drops_the_candidate() {
    let live = Live::start(|_base, root| write_transcript(&root.join("p/s.jsonl"), "doomed")).await;
    let path = live.root.join("p/s.jsonl");

    File::create(&path).expect("truncate");

    live.wait_until_gone(&path).await;
}

#[tokio::test]
async fn rotation_indexes_the_archive_and_the_fresh_transcript() {
    let live = Live::start(|_base, root| write_transcript(&root.join("p/s.jsonl"), "first")).await;
    let path = live.root.join("p/s.jsonl");
    let archive = live.root.join("p/s.1.jsonl");

    fs::rename(&path, &archive).expect("rotate");
    write_transcript(&path, "second");

    live.wait_for_session(&path, "second").await;
    live.wait_for_session(&archive, "first").await;
}

#[tokio::test]
async fn deleting_a_transcript_drops_the_candidate() {
    let live = Live::start(|_base, root| write_transcript(&root.join("p/s.jsonl"), "gone")).await;
    let path = live.root.join("p/s.jsonl");

    fs::remove_file(&path).expect("delete");

    live.wait_until_gone(&path).await;
}

#[tokio::test]
async fn a_recreated_directory_is_followed() {
    let live = Live::start(|_base, root| write_transcript(&root.join("p/old.jsonl"), "old")).await;
    let old = live.root.join("p/old.jsonl");
    let fresh = live.root.join("p/fresh.jsonl");

    fs::remove_dir_all(live.root.join("p")).expect("remove directory");
    live.wait_until_gone(&old).await;
    write_transcript(&fresh, "fresh");

    live.wait_for_session(&fresh, "fresh").await;

    let later = live.root.join("p/later.jsonl");
    write_transcript(&later, "later");
    live.wait_for_session(&later, "later").await;
}

#[tokio::test]
async fn a_directory_moved_into_the_root_is_indexed() {
    let live = Live::start(|base, root| {
        fs::create_dir_all(root).expect("root");
        write_transcript(&base.join("staging/deep/s.jsonl"), "moved");
    })
    .await;

    fs::rename(live.base.path().join("staging"), live.root.join("moved")).expect("move in");

    live.wait_for_session(&live.root.join("moved/deep/s.jsonl"), "moved")
        .await;
}

#[tokio::test]
async fn a_root_missing_at_start_is_picked_up_when_it_appears() {
    let live = Live::start(|_base, _root| {}).await;
    assert_eq!(
        live.handle.state(),
        TranscriptWatch::Degraded(WatcherDegraded::RootsMissing)
    );
    let path = live.root.join("p/s.jsonl");

    write_transcript(&path, "late");
    live.handle.request_reconcile();

    live.wait_for_state("active", |state| state == TranscriptWatch::Active)
        .await;
    live.wait_for_session(&path, "late").await;
}

#[tokio::test]
async fn a_recreated_root_is_watched_again() {
    let live = Live::start(|_base, root| write_transcript(&root.join("p/s.jsonl"), "before")).await;
    let before = live.root.join("p/s.jsonl");
    assert_eq!(live.handle.state(), TranscriptWatch::Active);

    fs::remove_dir_all(&live.root).expect("remove root");
    live.handle.request_reconcile();
    live.wait_for_state("roots missing", |state| {
        state == TranscriptWatch::Degraded(WatcherDegraded::RootsMissing)
    })
    .await;
    live.wait_until_gone(&before).await;

    let after = live.root.join("p/after.jsonl");
    write_transcript(&after, "after");
    live.handle.request_reconcile();
    live.wait_for_state("active", |state| state == TranscriptWatch::Active)
        .await;
    live.wait_for_session(&after, "after").await;

    let live_write = live.root.join("p/live.jsonl");
    write_transcript(&live_write, "live");
    live.wait_for_session(&live_write, "live").await;
}

/// Restores directory permissions so the fixture can be removed.
struct RestoreMode(PathBuf);

impl Drop for RestoreMode {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(RESTORED_DIR_MODE));
    }
}

#[tokio::test]
async fn an_unreadable_directory_is_reported_without_reading_outside_the_root() {
    let mut locked = PathBuf::new();
    let live = Live::start(|base, root| {
        write_transcript(&root.join("open/o.jsonl"), "open");
        write_transcript(&root.join("locked/l.jsonl"), "locked");
        write_transcript(&base.join("outside/x.jsonl"), "outside");
        locked = root.join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(DENIED_DIR_MODE))
            .expect("deny directory");
    })
    .await;
    let _restore = RestoreMode(locked.clone());

    if fs::read_dir(&locked).is_ok() {
        // Permission bits do not bind this user (root or a permissive test
        // sandbox), so the denial this test needs cannot be produced. Fail
        // loudly unless that is exactly the situation.
        assert_eq!(
            live.handle.state(),
            TranscriptWatch::Active,
            "a readable directory must not degrade the watcher"
        );
        eprintln!("denied-directory scenario not reproducible: permission bits do not apply");
        return;
    }

    assert!(
        matches!(
            live.handle.state(),
            TranscriptWatch::Degraded(
                WatcherDegraded::RegistrationFailed | WatcherDegraded::ScanIncomplete
            )
        ),
        "unexpected state {:?}",
        live.handle.state()
    );
    assert_eq!(
        live.session_id(&live.root.join("open/o.jsonl")).as_deref(),
        Some("open"),
        "readable siblings are still indexed"
    );
    assert!(!live.is_indexed(&locked.join("l.jsonl")));
    assert!(
        !live.is_indexed(&live.base.path().join("outside/x.jsonl")),
        "paths outside the root are never read"
    );

    #[cfg(target_os = "linux")]
    {
        fs::set_permissions(&locked, fs::Permissions::from_mode(RESTORED_DIR_MODE))
            .expect("restore directory");
        live.handle.request_reconcile();
        live.wait_for_state("active", |state| state == TranscriptWatch::Active)
            .await;
        live.wait_for_session(&locked.join("l.jsonl"), "locked")
            .await;
    }
}

#[tokio::test]
async fn a_root_replaced_by_a_file_drops_its_candidates() {
    let live = Live::start(|_base, root| write_transcript(&root.join("p/s.jsonl"), "stale")).await;
    let path = live.root.join("p/s.jsonl");
    assert!(live.is_indexed(&path));

    fs::remove_dir_all(&live.root).expect("remove root");
    fs::write(&live.root, b"not a directory").expect("replace with file");
    live.handle.request_reconcile();

    live.wait_until_gone(&path).await;
    live.wait_for_state("roots missing", |state| {
        state == TranscriptWatch::Degraded(WatcherDegraded::RootsMissing)
    })
    .await;
}

#[tokio::test]
async fn a_parent_replaced_by_a_file_or_transcript_by_a_directory_drops_the_candidate() {
    let live = Live::start(|_base, root| {
        write_transcript(&root.join("p/s.jsonl"), "one");
        write_transcript(&root.join("q/t.jsonl"), "two");
    })
    .await;
    let under_file = live.root.join("p/s.jsonl");
    let as_dir = live.root.join("q/t.jsonl");

    fs::remove_dir_all(live.root.join("p")).expect("remove p");
    fs::write(live.root.join("p"), b"file").expect("p becomes a file");
    fs::remove_file(&as_dir).expect("remove transcript");
    fs::create_dir(&as_dir).expect("transcript becomes a directory");
    live.wait_until_gone(&under_file).await;
    live.wait_until_gone(&as_dir).await;
}

#[tokio::test]
async fn an_indexed_transcript_replaced_by_a_directory_is_dropped() {
    let live = Live::start(|_base, root| write_transcript(&root.join("p/s.jsonl"), "stale")).await;
    let path = live.root.join("p/s.jsonl");
    assert!(live.is_indexed(&path));

    fs::remove_file(&path).expect("remove transcript");
    fs::create_dir(&path).expect("directory takes the name");

    live.wait_until_gone(&path).await;
}

#[tokio::test]
async fn a_redirected_symlinked_root_is_followed() {
    let live = Live::start(|base, root| {
        write_transcript(&base.join("a/p/old.jsonl"), "old");
        write_transcript(&base.join("b/p/new.jsonl"), "new");
        std::os::unix::fs::symlink(base.join("a"), root).expect("root link");
    })
    .await;
    let old = live.root.join("p/old.jsonl");
    let new = live.root.join("p/new.jsonl");
    assert!(live.is_indexed(&old));
    assert!(!live.is_indexed(&new));

    let staged = live.base.path().join("projects.new");
    std::os::unix::fs::symlink(live.base.path().join("b"), &staged).expect("staged link");
    fs::rename(&staged, &live.root).expect("redirect atomically");
    live.handle.request_reconcile();

    live.wait_for_session(&new, "new").await;
    live.wait_until_gone(&old).await;

    let later = live.root.join("p/later.jsonl");
    fs::write(live.base.path().join("b/p/later.jsonl"), record("later")).expect("write later");
    live.wait_for_session(&later, "later").await;
}

#[tokio::test]
async fn a_nested_root_keeps_its_own_agent_kind_in_scans_and_live_events() {
    let base = pohunek_test_support::tempdir().expect("base");
    let outer = base.path().join("claude");
    let inner = outer.join("codex");
    write_transcript(&outer.join("p/c.jsonl"), "claude-session");
    write_transcript(&inner.join("2026/x.jsonl"), "codex-session");
    let sessions = ExternalSessions::new();
    let roots = vec![
        TranscriptRoot {
            agent_base: RuntimeRef::claude(),
            path: outer.clone(),
        },
        TranscriptRoot {
            agent_base: RuntimeRef::codex(),
            path: inner.clone(),
        },
    ];

    let (index, _handle) = start_transcript_index(&roots, &sessions).await;
    let agent_of = |path: &Path| {
        index
            .inner
            .lock()
            .expect("index")
            .get(path)
            .map(|candidate| candidate.agent_base.clone())
    };

    assert_eq!(
        agent_of(&outer.join("p/c.jsonl")),
        Some(RuntimeRef::claude())
    );
    assert_eq!(
        agent_of(&inner.join("2026/x.jsonl")),
        Some(RuntimeRef::codex())
    );

    let live = inner.join("2026/live.jsonl");
    write_transcript(&live, "codex-live");
    let wait = async {
        while agent_of(&live).is_none() {
            sessions.inner.rescan.notified().await;
        }
    };
    tokio::time::timeout(LIVE_TEST_TIMEOUT, wait)
        .await
        .expect("live transcript indexed");
    assert_eq!(agent_of(&live), Some(RuntimeRef::codex()));
    sessions.shutdown();
}

#[tokio::test]
async fn a_transcript_that_becomes_readable_is_indexed_without_a_content_change() {
    use std::os::unix::fs::OpenOptionsExt;

    let live = Live::start(|_base, root| {
        fs::create_dir_all(root.join("p")).expect("project");
        // Owner write-only: the creator can fill it, nobody can read it back.
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(WRITE_ONLY_FILE_MODE)
            .open(root.join("p/s.jsonl"))
            .expect("create unreadable transcript");
        file.write_all(record("readable-later").as_bytes())
            .expect("fill transcript");
    })
    .await;
    let path = live.root.join("p/s.jsonl");
    if File::open(&path).is_ok() {
        eprintln!("unreadable-transcript scenario not reproducible: permission bits do not apply");
        return;
    }
    assert!(!live.is_indexed(&path));
    assert_eq!(
        live.handle.state(),
        TranscriptWatch::Degraded(WatcherDegraded::ScanIncomplete)
    );

    fs::set_permissions(&path, fs::Permissions::from_mode(READABLE_FILE_MODE))
        .expect("make readable");

    live.wait_for_session(&path, "readable-later").await;
}
