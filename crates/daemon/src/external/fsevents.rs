//! macOS `FSEvents` backend for live transcript watching.
//!
//! `FSEvents` watches a directory tree natively, so one recursive stream per
//! provider root replaces inotify's per-directory watches. The `notify` crate
//! owns the stream and its run-loop thread; its callback only forwards events
//! into a bounded queue. Dropped events, kernel/user drop flags, callback
//! errors and a full queue all surface as [`WatchEvent::Rescan`], so the runner
//! reconciles the index instead of trusting an incomplete event history.

// Rust guideline compliant 2026-09-29

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use notify::event::{CreateKind, ModifyKind, RemoveKind};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{mpsc, Notify};
use tokio_util::sync::CancellationToken;

use super::watch::{RootRegistration, WatchBackend, WatchEvent};
use super::{is_absent_error, is_jsonl_path};

/// Cause reported when the `FSEvents` watcher cannot be created.
pub(super) const OPEN_FAILED_CAUSE: &str = "fsevents_open_failed";

/// Maximum number of undelivered `FSEvents` callbacks held for the runner.
///
/// The callback runs on the `FSEvents` thread and must not block, so a full queue
/// drops the callback and raises the lost-events flag, which becomes a rescan.
const FSEVENTS_QUEUE_CAPACITY: usize = 4096;

/// Item forwarded from the `FSEvents` callback thread.
#[derive(Debug)]
enum RawSignal {
    Event(Event),
    Error,
}

/// One registered root and the canonical path `FSEvents` reports events under.
#[derive(Debug)]
struct Registered {
    configured: PathBuf,
    canonical: PathBuf,
}

/// Recursive `FSEvents` watch over the provider transcript roots.
///
/// The locked `notify` crate discards errors from starting the stream, so a
/// stream that silently failed to start delivers nothing. That only costs
/// latency: the periodic reconciliation pass keeps the index current.
#[derive(Debug)]
pub(super) struct FsEventsBackend {
    watcher: RecommendedWatcher,
    receiver: mpsc::Receiver<RawSignal>,
    lost: Arc<AtomicBool>,
    lost_wake: Arc<Notify>,
    registered: Vec<Registered>,
}

impl FsEventsBackend {
    /// Creates the `FSEvents` watcher without watching anything yet.
    pub(super) fn open() -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel(FSEVENTS_QUEUE_CAPACITY);
        let lost = Arc::new(AtomicBool::new(false));
        let lost_wake = Arc::new(Notify::new());
        let callback_lost = Arc::clone(&lost);
        let callback_wake = Arc::clone(&lost_wake);
        let watcher = RecommendedWatcher::new(
            move |result: notify::Result<Event>| {
                let signal = match result {
                    Ok(event) => RawSignal::Event(event),
                    Err(_error) => RawSignal::Error,
                };
                if sender.try_send(signal).is_err() {
                    callback_lost.store(true, Ordering::Release);
                    callback_wake.notify_one();
                }
            },
            Config::default(),
        )
        .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(Self {
            watcher,
            receiver,
            lost,
            lost_wake,
            registered: Vec::new(),
        })
    }

    fn translate(&self, signal: RawSignal, out: &mut Vec<WatchEvent>) {
        match signal {
            RawSignal::Error => out.push(WatchEvent::Rescan),
            RawSignal::Event(event) => translate_event(&self.registered, event, out),
        }
    }
}

/// Maps one `FSEvents` event to hints, rewriting canonical paths back under the
/// configured roots.
fn translate_event(registered: &[Registered], event: Event, out: &mut Vec<WatchEvent>) {
    if event.need_rescan() {
        out.push(WatchEvent::Rescan);
        return;
    }
    for path in event.paths {
        let path = configured_path(registered, path);
        match classify(event.kind, &path) {
            Hint::File => out.push(WatchEvent::Changed(path)),
            Hint::Tree => out.push(WatchEvent::TreeChanged(path)),
            Hint::Both => {
                out.push(WatchEvent::Changed(path.clone()));
                out.push(WatchEvent::TreeChanged(path));
            }
        }
    }
}

/// How an event about one path must be handled.
#[derive(Debug, PartialEq, Eq)]
enum Hint {
    File,
    Tree,
    Both,
}

/// Classifies an event by its kind and by what the path is now, never by its
/// name: a directory called `x.jsonl` is a tree change like any other directory.
///
/// Rename and untyped events do not say whether a file or a directory moved. A
/// path that is a directory now is a tree change; a `.jsonl` path that no longer
/// exists may have been either, so both hints are sent.
fn classify(kind: EventKind, path: &Path) -> Hint {
    match kind {
        EventKind::Create(CreateKind::Folder) | EventKind::Remove(RemoveKind::Folder) => Hint::Tree,
        EventKind::Create(CreateKind::File) | EventKind::Remove(RemoveKind::File) => Hint::File,
        // Permission or ownership changes can make a transcript readable, or a
        // directory listable, without any content change.
        EventKind::Modify(ModifyKind::Metadata(_)) => match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => Hint::Tree,
            _ => Hint::File,
        },
        EventKind::Modify(ModifyKind::Name(_)) | EventKind::Create(_) | EventKind::Remove(_) => {
            match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.is_dir() => Hint::Tree,
                Ok(_) if is_jsonl_path(path) => Hint::File,
                Err(_) if is_jsonl_path(path) => Hint::Both,
                Ok(_) | Err(_) => Hint::Tree,
            }
        }
        _ => Hint::File,
    }
}

/// Rewrites a path reported under a canonical root to the configured root of the
/// deepest registered root containing it.
fn configured_path(registered: &[Registered], path: PathBuf) -> PathBuf {
    registered
        .iter()
        .filter(|root| path.starts_with(&root.canonical))
        .max_by_key(|root| root.canonical.components().count())
        .and_then(|root| {
            path.strip_prefix(&root.canonical)
                .ok()
                .map(|rest| root.configured.join(rest))
        })
        .unwrap_or(path)
}

impl WatchBackend for FsEventsBackend {
    fn register_root(&mut self, root: &Path, _cancel: &CancellationToken) -> RootRegistration {
        let canonical = match fs::canonicalize(root) {
            Ok(canonical) => canonical,
            Err(error) if is_absent_error(&error) => {
                return RootRegistration::Missing;
            }
            Err(error) => return RootRegistration::Failed(error),
        };
        if !canonical.is_dir() {
            return RootRegistration::Missing;
        }
        // The `FSEvents` binding converts paths with `to_str().unwrap()`.
        if canonical.to_str().is_none() {
            return RootRegistration::Failed(io::Error::new(
                io::ErrorKind::InvalidInput,
                "transcript root path is not valid UTF-8",
            ));
        }
        if let Some(existing) = self
            .registered
            .iter()
            .find(|entry| entry.configured == root)
        {
            if existing.canonical == canonical {
                return RootRegistration::Complete;
            }
            let stale = existing.canonical.clone();
            let _ = self.watcher.unwatch(&stale);
            self.registered.retain(|entry| entry.configured != root);
        }
        match self.watcher.watch(&canonical, RecursiveMode::Recursive) {
            Ok(()) => {
                self.registered.push(Registered {
                    configured: root.to_path_buf(),
                    canonical,
                });
                RootRegistration::Complete
            }
            Err(error) => match error.kind {
                notify::ErrorKind::PathNotFound => RootRegistration::Missing,
                notify::ErrorKind::Io(io_error) => RootRegistration::Failed(io_error),
                notify::ErrorKind::MaxFilesWatch => RootRegistration::Failed(io::Error::new(
                    io::ErrorKind::QuotaExceeded,
                    "OS file watch limit reached",
                )),
                other => RootRegistration::Failed(io::Error::other(format!("{other:?}"))),
            },
        }
    }

    fn restart(&mut self) -> io::Result<()> {
        *self = Self::open()?;
        Ok(())
    }

    fn unregister_all(&mut self) {
        for entry in self.registered.drain(..) {
            let _ = self.watcher.unwatch(&entry.canonical);
        }
    }

    async fn next_events(&mut self, out: &mut Vec<WatchEvent>) -> io::Result<()> {
        tokio::select! {
            signal = self.receiver.recv() => {
                let Some(signal) = signal else {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "FSEvents event channel closed",
                    ));
                };
                self.translate(signal, out);
            }
            () = self.lost_wake.notified() => {}
        }
        for _ in 0..FSEVENTS_QUEUE_CAPACITY {
            let Ok(signal) = self.receiver.try_recv() else {
                break;
            };
            self.translate(signal, out);
        }
        if self.lost.swap(false, Ordering::AcqRel) {
            out.push(WatchEvent::Rescan);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use notify::event::{CreateKind, Flag, ModifyKind, RemoveKind, RenameMode};
    use notify::{Event, EventKind};

    use super::{translate_event, Registered, WatchEvent};

    fn registered() -> Vec<Registered> {
        vec![Registered {
            configured: PathBuf::from("/var/x/projects"),
            canonical: PathBuf::from("/private/var/x/projects"),
        }]
    }

    fn translate_with(registered: &[Registered], event: Event) -> Vec<WatchEvent> {
        let mut out = Vec::new();
        translate_event(registered, event, &mut out);
        out
    }

    fn translate(event: Event) -> Vec<WatchEvent> {
        translate_with(&registered(), event)
    }

    fn identity_roots(root: &std::path::Path) -> Vec<Registered> {
        vec![Registered {
            configured: root.to_path_buf(),
            canonical: root.to_path_buf(),
        }]
    }

    #[test]
    fn rescan_flag_becomes_a_rescan_hint() {
        let event = Event::new(EventKind::Other).set_flag(Flag::Rescan);

        assert_eq!(translate(event), vec![WatchEvent::Rescan]);
    }

    #[test]
    fn file_events_are_reported_under_the_configured_root() {
        let event = Event::new(EventKind::Create(CreateKind::File))
            .add_path(PathBuf::from("/private/var/x/projects/a/s.jsonl"));

        assert_eq!(
            translate(event),
            vec![WatchEvent::Changed(PathBuf::from(
                "/var/x/projects/a/s.jsonl"
            ))]
        );
    }

    #[test]
    fn directory_events_are_tree_changes_whatever_the_name() {
        for name in ["a", "x.jsonl"] {
            let dir = PathBuf::from(format!("/private/var/x/projects/{name}"));
            let expected = vec![WatchEvent::TreeChanged(PathBuf::from(format!(
                "/var/x/projects/{name}"
            )))];

            for kind in [
                EventKind::Create(CreateKind::Folder),
                EventKind::Remove(RemoveKind::Folder),
            ] {
                let event = Event::new(kind).add_path(dir.clone());
                assert_eq!(translate(event), expected, "{name} {kind:?}");
            }
        }
    }

    #[test]
    fn a_renamed_directory_named_like_a_transcript_is_a_tree_change() {
        let root = pohunek_test_support::tempdir().expect("root");
        let dir = root.path().join("x.jsonl");
        fs::create_dir_all(dir.join("nested")).expect("nested directory");
        let event =
            Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Any))).add_path(dir.clone());

        assert_eq!(
            translate_with(&identity_roots(root.path()), event),
            vec![WatchEvent::TreeChanged(dir)]
        );
    }

    #[test]
    fn a_renamed_transcript_file_stays_a_file_hint_and_a_vanished_one_is_both() {
        let root = pohunek_test_support::tempdir().expect("root");
        let present = root.path().join("s.jsonl");
        fs::write(&present, b"{}").expect("transcript");
        let gone = root.path().join("gone.jsonl");
        let roots = identity_roots(root.path());
        let rename = || EventKind::Modify(ModifyKind::Name(RenameMode::Any));

        assert_eq!(
            translate_with(&roots, Event::new(rename()).add_path(present.clone())),
            vec![WatchEvent::Changed(present)]
        );
        assert_eq!(
            translate_with(&roots, Event::new(rename()).add_path(gone.clone())),
            vec![
                WatchEvent::Changed(gone.clone()),
                WatchEvent::TreeChanged(gone)
            ]
        );
    }

    #[test]
    fn metadata_changes_are_file_hints_for_files_and_tree_changes_for_directories() {
        use notify::event::MetadataKind;

        let root = pohunek_test_support::tempdir().expect("root");
        let file = root.path().join("s.jsonl");
        fs::write(&file, b"{}").expect("transcript");
        let dir = root.path().join("sub");
        fs::create_dir(&dir).expect("directory");
        let roots = identity_roots(root.path());
        let kind = || EventKind::Modify(ModifyKind::Metadata(MetadataKind::Permissions));

        assert_eq!(
            translate_with(&roots, Event::new(kind()).add_path(file.clone())),
            vec![WatchEvent::Changed(file)]
        );
        assert_eq!(
            translate_with(&roots, Event::new(kind()).add_path(dir.clone())),
            vec![WatchEvent::TreeChanged(dir)]
        );
    }

    #[test]
    fn removed_transcripts_with_an_explicit_file_kind_stay_file_hints() {
        let path = PathBuf::from("/private/var/x/projects/a/s.jsonl");
        let event = Event::new(EventKind::Remove(RemoveKind::File)).add_path(path);

        assert_eq!(
            translate(event),
            vec![WatchEvent::Changed(PathBuf::from(
                "/var/x/projects/a/s.jsonl"
            ))]
        );
    }

    #[test]
    fn nested_roots_map_to_the_deepest_configured_root() {
        let registered = vec![
            Registered {
                configured: PathBuf::from("/home/u/.claude/projects"),
                canonical: PathBuf::from("/private/home/u/.claude/projects"),
            },
            Registered {
                configured: PathBuf::from("/home/u/codex-alias"),
                canonical: PathBuf::from("/private/home/u/.claude/projects/codex"),
            },
        ];
        let event = Event::new(EventKind::Create(CreateKind::File)).add_path(PathBuf::from(
            "/private/home/u/.claude/projects/codex/2026/s.jsonl",
        ));

        assert_eq!(
            translate_with(&registered, event),
            vec![WatchEvent::Changed(PathBuf::from(
                "/home/u/codex-alias/2026/s.jsonl"
            ))]
        );
    }
}
