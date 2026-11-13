//! Measures allocator calls and time of the worker output frame path per read.
//!
//! The counting global allocator counts every thread of the process, so this
//! binary holds exactly one test. Run it with:
//! `cargo nextest run --release -p pohunek-session-worker --test output_path_cost --run-ignored only --no-capture`

// Rust guideline compliant 2026-11-13

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use pohunek_session_worker::{write_output_event, OutputEvent, OutputHub};
use pohunek_worker_protocol::{StreamId, WorkerInstanceId, CURRENT_VERSION};

/// Allocator calls made by this process.
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

/// Bytes allocated by this process.
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

/// The system allocator with call and byte counters.
struct Counting;

#[expect(
    unsafe_code,
    reason = "a counting allocator wraps the system allocator"
)]
// SAFETY: every method forwards to `System` with the arguments it was given.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: the caller upholds the `GlobalAlloc::alloc` contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds the `GlobalAlloc::dealloc` contract.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        // SAFETY: the caller upholds the `GlobalAlloc::realloc` contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// Subscriber counts the bench iterates over.
const SUBSCRIBER_COUNTS: [usize; 3] = [1, 8, 32];

/// Equals the worker's default `data_payload_bytes`.
const DEFAULT_PAYLOAD_LIMIT: usize = 64 * 1024;

/// Forces multi-fragment terminal repaints.
const SMALL_PAYLOAD_LIMIT: usize = 1024;

/// Retained output bytes of the hub.
const HISTORY_BYTES: usize = 1 << 20;

/// Queued bytes per subscriber of the hub.
const SUBSCRIBER_BYTES: usize = 4 << 20;

/// Terminal rows of the hub.
const ROWS: u16 = 50;

/// Terminal columns of the hub.
const COLS: u16 = 200;

/// Printable bytes forming one 4 KiB read.
fn read_4k() -> Vec<u8> {
    vec![b'x'; 4 * 1024]
}

/// Printable bytes forming one 256 KiB read.
fn read_256k() -> Vec<u8> {
    vec![b'x'; 256 * 1024]
}

/// Fills the screen with `ROWS` lines of `COLS - 1` printable chars plus `\r\n`.
fn fill_screen() -> Vec<u8> {
    const LINE: usize = COLS as usize - 1;
    let mut bytes = Vec::with_capacity(ROWS as usize * (LINE + 2));
    for _ in 0..ROWS {
        bytes.extend_from_slice(&[b'y'; LINE]);
        bytes.extend_from_slice(b"\r\n");
    }
    bytes
}

async fn run_reads(
    hub: &OutputHub,
    subscribers: &mut [pohunek_session_worker::OutputSubscriber],
    payload: &[u8],
    reads: u64,
    stream_id: &StreamId,
    instance: &WorkerInstanceId,
) {
    for _ in 0..reads {
        hub.push(payload).expect("push read");
        for sub in &mut *subscribers {
            let event = sub.recv().await.expect("event");
            write_output_event(
                &mut tokio::io::sink(),
                CURRENT_VERSION,
                stream_id,
                instance,
                DEFAULT_PAYLOAD_LIMIT,
                event,
            )
            .await
            .expect("write");
        }
    }
}

/// Reads of the output measured loop with a 4 KiB payload.
const READS_4K: u64 = 10_000;

/// Reads of the output measured loop with a 256 KiB payload.
const READS_256K: u64 = 500;

/// Repaints of the terminal measured loop.
const REPAINTS: u64 = 200;

/// Bytes the ring warm-up pushes before the measured loop.
const WARMUP_BYTES: usize = 2 << 20;

/// Reads the ring warm-up pushes.
const WARMUP_READS: usize = WARMUP_BYTES / (4 * 1024);

async fn run_repaints(
    hub: &OutputHub,
    count: usize,
    repaints: u64,
    payload_limit: usize,
    stream_id: &StreamId,
    instance: &WorkerInstanceId,
) {
    for _ in 0..repaints {
        let mut subscribers = Vec::with_capacity(count);
        for _ in 0..count {
            subscribers.push(hub.subscribe_terminal_snapshot());
        }
        for sub in &mut subscribers {
            loop {
                let event = sub.recv().await.expect("snapshot event");
                match event {
                    OutputEvent::TerminalSnapshot(chunk) => {
                        let last = chunk.offset + chunk.bytes.len() == chunk.total_bytes;
                        write_output_event(
                            &mut tokio::io::sink(),
                            CURRENT_VERSION,
                            stream_id,
                            instance,
                            payload_limit,
                            OutputEvent::TerminalSnapshot(chunk),
                        )
                        .await
                        .expect("write");
                        if last {
                            break;
                        }
                    }
                    other => panic!("expected terminal snapshot, got {other:?}"),
                }
            }
        }
    }
}

/// Runs one output case for `count` subscribers and prints one line.
async fn bench_output(
    name: &str,
    count: usize,
    payload: &[u8],
    reads: u64,
    stream_id: &StreamId,
    instance: &WorkerInstanceId,
) {
    let hub = OutputHub::new(HISTORY_BYTES, SUBSCRIBER_BYTES, ROWS, COLS).expect("create hub");
    // Warm the ring with 2 MiB of pushes (4 KiB reads of printable bytes).
    let warm = read_4k();
    for _ in 0..WARMUP_READS {
        hub.push(&warm).expect("warm the ring");
    }
    let mut subscribers = Vec::with_capacity(count);
    for _ in 0..count {
        subscribers.push(hub.subscribe(Some(hub.next_offset())).expect("subscribe"));
    }
    for sub in &mut subscribers {
        let event = sub.recv().await.expect("initial empty replay marker");
        write_output_event(
            &mut tokio::io::sink(),
            CURRENT_VERSION,
            stream_id,
            instance,
            DEFAULT_PAYLOAD_LIMIT,
            event,
        )
        .await
        .expect("write initial marker");
    }

    // Warm-up of 100 iterations of the same loop.
    run_reads(&hub, &mut subscribers, payload, 100, stream_id, instance).await;

    let alloc_before = ALLOCATIONS.load(Ordering::Relaxed);
    let bytes_before = ALLOCATED_BYTES.load(Ordering::Relaxed);
    let start = Instant::now();
    run_reads(&hub, &mut subscribers, payload, reads, stream_id, instance).await;
    let elapsed = start.elapsed();
    let allocs = ALLOCATIONS.load(Ordering::Relaxed) - alloc_before;
    let bytes = ALLOCATED_BYTES.load(Ordering::Relaxed) - bytes_before;
    eprintln!(
        "case={name} M={count} allocs_per_unit={} bytes_per_unit={} ns_per_unit={}",
        allocs / reads.max(1),
        bytes / reads.max(1),
        elapsed.as_nanos() / u128::from(reads.max(1))
    );
}

/// Runs one terminal case for `count` subscribers and prints one line.
async fn bench_terminal(
    name: &str,
    count: usize,
    payload_limit: usize,
    stream_id: &StreamId,
    instance: &WorkerInstanceId,
) {
    let hub = OutputHub::new(HISTORY_BYTES, SUBSCRIBER_BYTES, ROWS, COLS).expect("create hub");
    hub.push(&fill_screen()).expect("fill the screen");

    // Warm-up of 100 iterations of the same loop.
    run_repaints(&hub, count, 100, payload_limit, stream_id, instance).await;

    let alloc_before = ALLOCATIONS.load(Ordering::Relaxed);
    let bytes_before = ALLOCATED_BYTES.load(Ordering::Relaxed);
    let start = Instant::now();
    run_repaints(&hub, count, REPAINTS, payload_limit, stream_id, instance).await;
    let elapsed = start.elapsed();
    let allocs = ALLOCATIONS.load(Ordering::Relaxed) - alloc_before;
    let bytes = ALLOCATED_BYTES.load(Ordering::Relaxed) - bytes_before;
    let divisor = REPAINTS * count as u64;
    eprintln!(
        "case={name} M={count} allocs_per_unit={} bytes_per_unit={} ns_per_unit={}",
        allocs / divisor,
        bytes / divisor,
        elapsed.as_nanos() / u128::from(divisor)
    );
}

#[tokio::test]
#[ignore = "measurement; run with --release --run-ignored only --no-capture"]
async fn output_path_allocations_and_time_per_read() {
    let stream_id = StreamId::new("bench-stream").expect("valid stream");
    let instance = WorkerInstanceId::new("bench-runtime").expect("valid runtime");
    let payload_4k = read_4k();
    let payload_256k = read_256k();
    for &count in &SUBSCRIBER_COUNTS {
        bench_output(
            "output 4KiB",
            count,
            &payload_4k,
            READS_4K,
            &stream_id,
            &instance,
        )
        .await;
        bench_output(
            "output 256KiB",
            count,
            &payload_256k,
            READS_256K,
            &stream_id,
            &instance,
        )
        .await;
        bench_terminal(
            "terminal 64KiB limit",
            count,
            DEFAULT_PAYLOAD_LIMIT,
            &stream_id,
            &instance,
        )
        .await;
        bench_terminal(
            "terminal 1KiB limit",
            count,
            SMALL_PAYLOAD_LIMIT,
            &stream_id,
            &instance,
        )
        .await;
    }
}
