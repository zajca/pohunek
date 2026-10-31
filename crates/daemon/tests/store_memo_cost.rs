//! A store write costs the same whatever the number of records.
//!
//! The scenario counts allocator calls with a counting global allocator, which
//! counts every thread of the process, so this test binary holds one test.

// Rust guideline compliant 2026-10-31

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use pohunek_daemon::store::{SchemaMigration, SessionRecord, Store};

/// Allocator calls made by this process.
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

/// The system allocator with a call counter.
struct Counting;

#[expect(
    unsafe_code,
    reason = "a counting allocator wraps the system allocator"
)]
// SAFETY: every method forwards to `System` with the arguments it was given.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the caller upholds the `GlobalAlloc::alloc` contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds the `GlobalAlloc::dealloc` contract.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the caller upholds the `GlobalAlloc::realloc` contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

const SCHEMA_10: &str = include_str!("../src/store/fixtures/schema-10/metadata.jsonl");

/// Session writes timed by the allocation scenario.
const MEASURED_WRITES: u64 = 10;

/// Records the small store of the allocation scenario holds.
const SMALL_STORE: usize = 50;

/// Records the large store of the allocation scenario holds.
const LARGE_STORE: usize = 400;

/// How much more, in percent of its cost, a write may cost on the large store
/// than on the small one.
const COST_GROWTH_BOUND_PERCENT: u64 = 150;

fn create_store(root: &Path, content: &str) -> (Store, PathBuf) {
    let path = root.join("metadata.jsonl");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("create owner-private store")
        .write_all(content.as_bytes())
        .expect("write the store");
    (Store::new(path.clone()), path)
}

/// The schema-10 fixture migrated to the current schema.
fn migrated_store(root: &Path) -> (Store, PathBuf) {
    let (store, path) = create_store(root, SCHEMA_10);
    assert!(matches!(
        store.migrate_to_current().expect("migrate previous schema"),
        SchemaMigration::Migrated { .. }
    ));
    (store, path)
}

fn fixture_session(store: &Store) -> SessionRecord {
    store
        .load_sessions()
        .expect("load sessions")
        .pop()
        .expect("the fixture session")
}

/// A copy of `base` under another session id.
fn numbered_session(base: &SessionRecord, number: usize) -> SessionRecord {
    let id = format!("s-memo-{number:04}");
    let mut record = base.clone();
    record.session_id.clone_from(&id);
    record.info.id = protocol::SessionId(id);
    record.recovery = None;
    record
}

/// Allocator calls of [`MEASURED_WRITES`] writes of one session record into a
/// store of `records` sessions, per write.
fn allocations_per_write(records: usize) -> u64 {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let (store, _path) = migrated_store(root.path());
    let base = fixture_session(&store);
    for number in 0..records {
        store
            .record_session(&numbered_session(&base, number))
            .expect("seed a session");
    }
    let mut record = numbered_session(&base, 0);
    // The first write after the seeding may decode the store once.
    store.record_session(&record).expect("warm the store");
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for write in 0..MEASURED_WRITES {
        record.info.updated_at = format!("2026-10-31T00:00:{write:02}Z");
        store.record_session(&record).expect("write the session");
    }
    let calls = ALLOCATIONS.load(Ordering::Relaxed) - before;
    calls / MEASURED_WRITES
}

#[test]
fn a_write_costs_the_same_on_a_large_store_as_on_a_small_one() {
    let small = allocations_per_write(SMALL_STORE);
    let large = allocations_per_write(LARGE_STORE);
    assert!(
        large * 100 <= small * COST_GROWTH_BOUND_PERCENT,
        "allocator calls per write: {small} with {SMALL_STORE} sessions, {large} with {LARGE_STORE}"
    );
}
