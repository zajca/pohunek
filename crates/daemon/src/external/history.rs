//! One-shot, read-only scan of agent CLI history for `project.candidates`.
//!
//! The roots are the transcript trees the installed runtime packages declare in
//! `[transcripts]`; nothing else is opened. From each `jsonl-head-v1` history
//! file the scan keeps the working directory of the first record that has one
//! and the file's modification time. Nothing else of a file is retained or
//! returned, and a record is parsed only to read those two values.
//!
//! The walk never follows a symlink: a linked directory is not entered, a
//! linked file is not opened, and a file is opened with `O_NOFOLLOW` and
//! checked to be a regular file after the open.

// Rust guideline compliant 2026-10-12

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Instant, SystemTime};

use protocol::{CandidateLimit, RuntimeRef};
use serde_json::Value;
use tracing::debug;

use super::{is_jsonl_path, list_walked_directory, observed_roots, string_at, TranscriptRoot};
use crate::agent::host::{HeadKeyPath, TranscriptParser};
use crate::integration::ConfigHomes;
use crate::project::candidates::CandidateConfig;

/// Longest working directory the scan accepts, in bytes (`PATH_MAX`). A longer
/// value cannot name a directory and is skipped without a filesystem call.
const MAX_CWD_BYTES: usize = 4096;

/// The top-level record key that names the working directory.
const CWD_KEY: &str = "cwd";

/// What one history file contributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistoryRecord {
    /// The runtime whose history declares the file.
    pub(crate) agent: RuntimeRef,
    /// The working directory the first record carrying one names.
    pub(crate) cwd: PathBuf,
    /// The file's modification time.
    pub(crate) modified: SystemTime,
}

/// The outcome of one history scan.
#[derive(Debug, Default)]
pub(crate) struct HistoryScan {
    /// One record per history file that names a working directory.
    pub(crate) records: Vec<HistoryRecord>,
    /// The bounds that cut the scan short.
    pub(crate) limits: BTreeSet<CandidateLimit>,
}

/// A history file found by the walk, before it is read.
struct Found {
    path: PathBuf,
    modified: SystemTime,
    /// Index into the scan's roots.
    root: usize,
}

/// Walks the history trees `homes` declares and reads the working directory of
/// each file, within the bounds of `config` and until `deadline`.
///
/// The file bound keeps the newest files by modification time; the entry,
/// directory and time bounds end the walk wherever it stands.
pub(crate) fn scan(
    homes: &ConfigHomes,
    config: &CandidateConfig,
    deadline: Instant,
) -> HistoryScan {
    let (roots, _skipped) = observed_roots(homes.launch_homes());
    let roots: Vec<TranscriptRoot> = roots
        .into_iter()
        .filter(|root| root.parser == TranscriptParser::JsonlHeadV1)
        .collect();
    let mut scan = HistoryScan::default();
    let mut found = Vec::new();
    for (index, root) in roots.iter().enumerate() {
        // Each root has its own budget, so a large tree cannot starve another.
        let mut entries_left = config.max_entries;
        let mut directories_left = config.max_directories;
        walk(
            root,
            index,
            &mut found,
            &mut entries_left,
            &mut directories_left,
            deadline,
            &mut scan.limits,
        );
    }
    found.sort_by_key(|file| std::cmp::Reverse(file.modified));
    if found.len() > config.max_files {
        found.truncate(config.max_files);
        scan.limits.insert(CandidateLimit::Files);
    }
    for file in found {
        if Instant::now() >= deadline {
            scan.limits.insert(CandidateLimit::Time);
            break;
        }
        let root = &roots[file.root];
        match read_cwd(&file.path, root.head_paths.cwd(), config) {
            Ok(Head::Cwd(cwd)) => scan.records.push(HistoryRecord {
                agent: root.agent_base.clone(),
                cwd,
                modified: file.modified,
            }),
            Ok(Head::Cut) => {
                scan.limits.insert(CandidateLimit::LineLength);
            }
            Ok(Head::None) => {}
            Err(error) => {
                debug!(
                    name: "project.candidates.history_unreadable",
                    kind = ?error.kind(),
                    "a history file could not be read and was skipped"
                );
            }
        }
    }
    scan
}

/// Collects the regular `.jsonl` files below `root` into `found`, newest names
/// first, without following a symlink. `entries_left` and `directories_left`
/// are the budget of this root alone.
fn walk(
    root: &TranscriptRoot,
    index: usize,
    found: &mut Vec<Found>,
    entries_left: &mut usize,
    directories_left: &mut usize,
    deadline: Instant,
    limits: &mut BTreeSet<CandidateLimit>,
) {
    // The root itself is the user's own declared location and may be a link
    // (a dotfiles setup); everything below it is listed without following.
    let Ok(base) = fs::canonicalize(&root.path) else {
        return;
    };
    let mut incomplete = false;
    let mut stack = vec![(base, 0_u8)];
    while let Some((dir, depth)) = stack.pop() {
        if Instant::now() >= deadline {
            limits.insert(CandidateLimit::Time);
            return;
        }
        let Some(entries) = list_walked_directory(root, &dir, true, &mut incomplete) else {
            continue;
        };
        let mut subdirectories = Vec::new();
        let mut session_files = BTreeSet::new();
        for entry in entries {
            if *entries_left == 0 {
                limits.insert(CandidateLimit::Entries);
                return;
            }
            *entries_left -= 1;
            let Ok(entry) = entry else { continue };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                if root.depth_limit().is_some_and(|max| depth >= max) {
                    continue;
                }
                if *directories_left == 0 {
                    limits.insert(CandidateLimit::Entries);
                    return;
                }
                *directories_left -= 1;
                subdirectories.push(path);
            } else if file_type.is_file() && is_jsonl_path(&path) {
                let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) else {
                    continue;
                };
                if let Some(stem) = path.file_stem() {
                    session_files.insert(stem.to_owned());
                }
                found.push(Found {
                    path,
                    modified,
                    root: index,
                });
            }
        }
        // A directory named like a sibling session file holds that session's
        // auxiliary logs (a Claude conversation's `subagents/*.jsonl`), not
        // sessions of their own, so it is not entered.
        subdirectories.retain(|dir| {
            dir.file_name()
                .is_none_or(|name| !session_files.contains(name))
        });
        // Reversed so the first (newest-named) directory is popped first.
        stack.extend(
            subdirectories
                .into_iter()
                .rev()
                .map(|child| (child, depth.saturating_add(1))),
        );
    }
}

/// What the head of one history file yields.
#[derive(Debug, PartialEq, Eq)]
enum Head {
    /// A valid working directory.
    Cwd(PathBuf),
    /// No record in the head names one.
    None,
    /// The byte bound cut a record in two before any record named one.
    Cut,
}

/// Reads the working directory from the head of the history file at `path`.
///
/// At most `config.head_bytes` bytes and `config.max_records` records are
/// read. The file is opened without following a symlink and must be a regular
/// file. The first record that carries a valid working directory wins: its
/// top-level `cwd` key, or, for the first record only, the `cwd_path` the
/// runtime declares.
fn read_cwd(
    path: &Path,
    declared: Option<&HeadKeyPath>,
    config: &CandidateConfig,
) -> io::Result<Head> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Ok(Head::None);
    }
    let limit = u64::try_from(config.head_bytes).unwrap_or(u64::MAX);
    let mut head = Vec::new();
    file.take(limit).read_to_end(&mut head)?;
    let at_limit = head.len() >= config.head_bytes;
    for (index, line) in head
        .split_inclusive(|byte| *byte == b'\n')
        .take(config.max_records)
        .enumerate()
    {
        if at_limit && !line.ends_with(b"\n") {
            return Ok(Head::Cut);
        }
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let named = value
            .get(CWD_KEY)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| {
                declared
                    .filter(|_| index == 0)
                    .and_then(|keys| string_at(&value, keys))
            });
        if let Some(cwd) = named.and_then(|text| validated_cwd(&text)) {
            return Ok(Head::Cwd(cwd));
        }
    }
    Ok(Head::None)
}

/// `text` as a working directory, or `None` unless it is an absolute path of
/// plain components within [`MAX_CWD_BYTES`] and without a NUL byte.
fn validated_cwd(text: &str) -> Option<PathBuf> {
    if text.is_empty() || text.len() > MAX_CWD_BYTES || text.contains('\0') {
        return None;
    }
    let path = PathBuf::from(text);
    let plain = path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)));
    plain.then_some(path)
}
