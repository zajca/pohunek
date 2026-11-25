//! Scan bounds over a Cline session store, `<root>/<id>/<id>.json`.
//!
//! A pass that ends at a work bound must have visited the newest sessions:
//! Cline generates ids that start with their creation time in milliseconds, so
//! the highest directory names are the newest sessions.

// Rust guideline compliant 2026-11-25

use std::fs;
use std::path::{Path, PathBuf};

use protocol::RuntimeRef;
use tokio_util::sync::CancellationToken;

use super::watch::RootScan;
use super::{ScanLimits, TranscriptIndex, TranscriptRoot, SCAN_LIMITS};
use crate::agent::host::{HeadPaths, TranscriptParser};

/// Generated session ids, oldest first.
const IDS: [&str; 4] = [
    "1793924186890_aaaaa",
    "1793924186891_bbbbb",
    "1793924186892_ccccc",
    "1793924186893_ddddd",
];

/// A Cline session store below a new temporary root, one manifest per id.
fn store() -> tempfile::TempDir {
    let dir = pohunek_test_support::tempdir().expect("root");
    for id in IDS {
        let path = manifest(dir.path(), id);
        fs::create_dir_all(path.parent().expect("session directory")).expect("session directory");
        fs::write(
            &path,
            format!("{{\"session_id\":\"{id}\",\"cwd\":\"/work\"}}"),
        )
        .expect("write the manifest");
    }
    dir
}

fn manifest(root: &Path, id: &str) -> PathBuf {
    root.join(id).join(format!("{id}.json"))
}

/// The root of the store, read by the parser a package names by its id.
fn cline_root(path: &Path) -> TranscriptRoot {
    TranscriptRoot {
        parser: TranscriptParser::from_id("cline-session-v1").expect("the Cline parser"),
        head_paths: HeadPaths::default(),
        agent_base: RuntimeRef::from_wire("cline"),
        path: path.to_path_buf(),
        private: false,
        max_depth: None,
    }
}

/// Scans `root` with `limits` into a fresh index and returns the result with
/// the ids of the manifests it indexed.
fn scan(root: &TranscriptRoot, limits: ScanLimits) -> (RootScan, Vec<&'static str>) {
    let index = TranscriptIndex::default();
    let result = index.scan_root(
        root,
        std::slice::from_ref(root),
        limits,
        &CancellationToken::new(),
    );
    let indexed = index.inner.lock().expect("index");
    let ids = IDS
        .into_iter()
        .filter(|id| indexed.contains_key(&manifest(&root.path, id)))
        .collect();
    (result, ids)
}

#[test]
fn a_session_store_over_the_entry_bound_indexes_the_newest_sessions_first() {
    let dir = store();

    // The root lists its four session directories; the bound leaves room for
    // the manifests of two of them.
    let (result, indexed) = scan(
        &cline_root(dir.path()),
        ScanLimits {
            entries: IDS.len() + 2,
            ..SCAN_LIMITS
        },
    );

    assert_eq!(result, RootScan::Incomplete);
    assert_eq!(indexed, [IDS[2], IDS[3]]);
}

#[test]
fn a_session_store_over_the_directory_bound_indexes_the_newest_sessions_first() {
    let dir = store();

    // The bound ends while the root's own listing collects session
    // directories; the ones collected so far are the newest.
    let (result, indexed) = scan(
        &cline_root(dir.path()),
        ScanLimits {
            directories: 2,
            ..SCAN_LIMITS
        },
    );

    assert_eq!(result, RootScan::Incomplete);
    assert_eq!(indexed, [IDS[2], IDS[3]]);
}

#[test]
fn a_session_store_within_the_bounds_is_indexed_completely() {
    let dir = store();

    let (result, indexed) = scan(&cline_root(dir.path()), SCAN_LIMITS);

    assert_eq!(result, RootScan::Complete);
    assert_eq!(indexed, IDS);
}
