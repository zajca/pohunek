//! Executes ordered PTY input with bounded exact control deduplication.

// Rust guideline compliant 2026-10-13

use std::collections::{HashMap, VecDeque};
use std::fmt::{Debug, Formatter};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::sync::{watch, Mutex as AsyncMutex};

/// Bits retained only for protocol-v2 unordered input compatibility.
///
/// New lease-scoped monotonic control input never consults this filter.
const LEGACY_EVICTED_FILTER_WORDS: usize = 256;

/// One ordered write-plan fragment.
#[derive(Clone, PartialEq, Eq)]
pub struct InputFragment {
    /// Raw PTY bytes.
    pub bytes: Vec<u8>,
    /// Delay owned by the worker after flushing this fragment.
    pub delay_after: Duration,
}

impl Debug for InputFragment {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputFragment")
            .field(
                "bytes",
                &format_args!("[REDACTED; {} bytes]", self.bytes.len()),
            )
            .field("delay_after", &self.delay_after)
            .finish()
    }
}

/// Idempotent ordered input operation.
#[derive(Clone, PartialEq, Eq)]
pub struct InputPlan {
    /// Runtime-unique write identifier.
    pub write_id: String,
    /// Ordered fragments.
    pub fragments: Vec<InputFragment>,
}

impl Debug for InputPlan {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputPlan")
            .field("write_id", &self.write_id)
            .field("fragment_count", &self.fragments.len())
            .field(
                "payload_bytes",
                &self
                    .fragments
                    .iter()
                    .map(|fragment| fragment.bytes.len())
                    .sum::<usize>(),
            )
            .finish()
    }
}

/// Input-plan execution failure.
#[derive(Debug, thiserror::Error)]
pub enum InputError {
    /// Deduplication capacity is invalid.
    #[error("input deduplication capacity must be greater than zero")]
    InvalidCapacity,
    /// A write ID was reused with different content.
    #[error("write id `{write_id}` was reused with different content")]
    Conflict {
        /// Conflicting write ID.
        write_id: String,
    },
    /// A write at or below the lease watermark is no longer retained.
    #[error("write outcome is unknown for expired id `{write_id}`")]
    OutcomeUnknown {
        /// Ambiguous write ID.
        write_id: String,
    },
    /// The PTY writer failed.
    #[error("PTY input write failed: {0}")]
    Io(std::io::Error),
    /// The child accepted no input byte within the stall bound.
    ///
    /// `written` counts bytes accepted across the whole plan before the stall;
    /// the remaining fragments of the plan were not written.
    #[error(
        "PTY input made no progress for {} ms; {written} of {total} bytes reached the terminal",
        stall.as_millis()
    )]
    Stalled {
        /// Plan bytes accepted by the PTY before the stall.
        written: usize,
        /// Total bytes in the plan.
        total: usize,
        /// Stall bound that expired.
        stall: Duration,
    },
    /// A blocking writer task failed.
    #[error("PTY input writer task terminated unexpectedly")]
    WriterTask,
}

/// Byte sink for PTY input whose writes may be bounded by a stall.
pub trait InputWriter: Send {
    /// Writes a prefix of `buf` and returns how many bytes were accepted.
    ///
    /// `stall` bounds the time without an accepted byte. `None` waits for as
    /// long as the sink blocks.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::ErrorKind::TimedOut`] when `stall` expires without
    /// progress, and the sink's own error otherwise.
    fn write_within(&mut self, buf: &[u8], stall: Option<Duration>) -> std::io::Result<usize>;

    /// Flushes buffered bytes to the sink.
    ///
    /// # Errors
    ///
    /// Returns the sink's I/O error.
    fn flush(&mut self) -> std::io::Result<()>;
}

/// Cloneable ordered input coordinator.
#[derive(Clone)]
pub struct WriteCoordinator {
    writer: Arc<Mutex<Box<dyn InputWriter>>>,
    execution: Arc<AsyncMutex<()>>,
    state: Arc<Mutex<DedupState>>,
    /// Stall bound for control plans, reported in [`InputError::Stalled`].
    ///
    /// It bounds the wait for the execution lock and every PTY write of a
    /// control plan. Attach streams never use it.
    stall: Option<Duration>,
}

impl Debug for WriteCoordinator {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let state = lock(&self.state);
        f.debug_struct("WriteCoordinator")
            .field("retained_entries", &state.entries.len())
            .field("namespace_id", &state.namespace_id)
            .field("high_watermark", &state.high_watermark)
            .field("capacity", &state.capacity)
            .finish_non_exhaustive()
    }
}

impl WriteCoordinator {
    /// Creates a coordinator around a real writer.
    ///
    /// `stall` is the control-plan bound. Attach streams are unbounded whatever
    /// its value.
    ///
    /// # Errors
    ///
    /// Returns [`InputError::InvalidCapacity`] when `capacity` is zero.
    pub fn new(
        writer: impl InputWriter + 'static,
        capacity: usize,
        stall: Option<Duration>,
    ) -> Result<Self, InputError> {
        if capacity == 0 {
            return Err(InputError::InvalidCapacity);
        }
        Ok(Self {
            writer: Arc::new(Mutex::new(Box::new(writer))),
            execution: Arc::new(AsyncMutex::new(())),
            stall,
            state: Arc::new(Mutex::new(DedupState {
                entries: HashMap::new(),
                completed: VecDeque::new(),
                namespace_id: None,
                mode: None,
                high_watermark: 0,
                legacy_evicted: EvictedFilter::new(),
                capacity,
            })),
        })
    }

    /// Executes or joins an idempotent control write plan.
    ///
    /// Completion is published only after all fragments are flushed. Plans are
    /// globally serialized so their delayed fragments cannot interleave. The
    /// lease sequence must increase in request order. A changed lease starts a
    /// fresh namespace because the previous lease is rejected before input
    /// handling.
    ///
    /// The plan is bounded by the coordinator's stall: waiting for the
    /// execution lock held by an attach write and every PTY write count as no
    /// progress. A control caller must never wedge its connection behind a
    /// human's input, so a plan that cannot start within the stall fails with
    /// [`InputError::Stalled`] and writes nothing.
    ///
    /// # Errors
    ///
    /// Returns [`InputError`] for conflicting, ambiguous, or failed writes.
    pub async fn execute_control(
        &self,
        namespace_id: &str,
        sequence: u64,
        plan: InputPlan,
    ) -> Result<(), InputError> {
        let fingerprint = fingerprint(&plan);
        let action = {
            let mut state = lock(&self.state);
            state.select_namespace(namespace_id, DedupMode::Monotonic);
            match state.entries.get(&plan.write_id) {
                Some(entry) if entry.fingerprint != fingerprint => {
                    return Err(InputError::Conflict {
                        write_id: plan.write_id,
                    });
                }
                Some(entry) => Action::Join(entry.result.clone()),
                None if sequence <= state.high_watermark => {
                    return Err(InputError::OutcomeUnknown {
                        write_id: plan.write_id,
                    });
                }
                None => {
                    state.high_watermark = sequence;
                    let (result_tx, result_rx) = watch::channel(None);
                    state.entries.insert(
                        plan.write_id.clone(),
                        DedupEntry {
                            fingerprint,
                            result: result_rx,
                        },
                    );
                    Action::Execute(result_tx)
                }
            }
        };

        match action {
            Action::Join(receiver) => wait_result(receiver).await,
            Action::Execute(sender) => {
                let result = self.execute_once(&plan.fragments, self.stall).await;
                let stored = result.as_ref().copied().map_err(StoredError::from_input);
                self.complete(&plan.write_id);
                let _ = sender.send(Some(stored));
                result
            }
        }
    }

    /// Executes a protocol-v2 input plan with its original bounded semantics.
    ///
    /// Version-two daemons allocated IDs before entering the per-worker request
    /// mutex, so two fresh IDs could arrive out of order. This compatibility
    /// path therefore cannot use a high watermark. New daemon clients never use
    /// it.
    ///
    /// # Errors
    ///
    /// Returns [`InputError`] for conflicting, ambiguous, or failed writes.
    pub async fn execute_legacy(
        &self,
        namespace_id: &str,
        plan: InputPlan,
    ) -> Result<(), InputError> {
        let fingerprint = fingerprint(&plan);
        let action = {
            let mut state = lock(&self.state);
            state.select_namespace(namespace_id, DedupMode::Legacy);
            match state.entries.get(&plan.write_id) {
                Some(entry) if entry.fingerprint != fingerprint => {
                    return Err(InputError::Conflict {
                        write_id: plan.write_id,
                    });
                }
                Some(entry) => Action::Join(entry.result.clone()),
                None if state.legacy_evicted.might_contain(&plan.write_id) => {
                    return Err(InputError::OutcomeUnknown {
                        write_id: plan.write_id,
                    });
                }
                None => {
                    let (result_tx, result_rx) = watch::channel(None);
                    state.entries.insert(
                        plan.write_id.clone(),
                        DedupEntry {
                            fingerprint,
                            result: result_rx,
                        },
                    );
                    Action::Execute(result_tx)
                }
            }
        };

        match action {
            Action::Join(receiver) => wait_result(receiver).await,
            Action::Execute(sender) => {
                let result = self.execute_once(&plan.fragments, self.stall).await;
                let stored = result.as_ref().copied().map_err(StoredError::from_input);
                self.complete(&plan.write_id);
                let _ = sender.send(Some(stored));
                result
            }
        }
    }

    /// Executes input ordered by one attach stream.
    ///
    /// The attach stream is a human's terminal. Its writes and its wait for
    /// the execution lock are unbounded, so a paste into a busy foreground
    /// program is delivered when the program reads it, as in a real terminal,
    /// and does not end the stream. Control plans are bounded instead, so a
    /// stalled attach write only delays them up to the stall bound.
    ///
    /// The stream server validates strictly increasing stream-scoped sequence
    /// identifiers before calling this method. Framed stream delivery is
    /// ordered and never replayed by the daemon, so lifetime deduplication would
    /// add ambiguity without adding retry safety.
    ///
    /// The write runs in a task of its own, so dropping the returned future
    /// leaves it running: the task holds the execution lock until the PTY
    /// accepted every byte, and a control plan waiting behind it keeps its
    /// stall bound.
    ///
    /// # Errors
    ///
    /// Returns [`InputError`] when the PTY writer fails.
    pub async fn execute_stream(&self, fragments: Vec<InputFragment>) -> Result<(), InputError> {
        let coordinator = self.clone();
        tokio::spawn(async move { coordinator.execute_once(&fragments, None).await })
            .await
            .map_err(|_join_error| InputError::WriterTask)?
    }

    /// Writes the fragments in order while holding the execution lock.
    ///
    /// `stall` bounds the lock wait and every write. `None` leaves both
    /// unbounded.
    async fn execute_once(
        &self,
        fragments: &[InputFragment],
        stall: Option<Duration>,
    ) -> Result<(), InputError> {
        let total = fragments.iter().map(|fragment| fragment.bytes.len()).sum();
        let _execution = match stall {
            Some(stall) => match tokio::time::timeout(stall, self.execution.lock()).await {
                Ok(guard) => guard,
                Err(_elapsed) => {
                    return Err(InputError::Stalled {
                        written: 0,
                        total,
                        stall,
                    });
                }
            },
            None => self.execution.lock().await,
        };
        let mut written = 0;
        for fragment in fragments {
            let bytes = fragment.bytes.clone();
            let writer = Arc::clone(&self.writer);
            let (accepted, result) = tokio::task::spawn_blocking(move || {
                let mut writer = lock(&writer);
                let (accepted, result) = write_counted(&mut **writer, &bytes, stall);
                (accepted, result.and_then(|()| writer.flush()))
            })
            .await
            .map_err(|_join_error| InputError::WriterTask)?;
            written += accepted;
            match result {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                    let Some(stall) = stall else {
                        return Err(InputError::Io(error));
                    };
                    return Err(InputError::Stalled {
                        written,
                        total,
                        stall,
                    });
                }
                Err(error) => return Err(InputError::Io(error)),
            }
            if !fragment.delay_after.is_zero() {
                tokio::time::sleep(fragment.delay_after).await;
            }
        }
        Ok(())
    }

    fn complete(&self, write_id: &str) {
        let mut state = lock(&self.state);
        state.completed.push_back(write_id.to_owned());
        while state.completed.len() > state.capacity {
            let Some(evicted_id) = state.completed.pop_front() else {
                break;
            };
            state.entries.remove(&evicted_id);
            if state.mode == Some(DedupMode::Legacy) {
                state.legacy_evicted.insert(&evicted_id);
            }
        }
    }
}

/// Writes all of `bytes` and reports how many were accepted before any failure.
///
/// The count stays exact on failure, which lets a stalled plan report progress.
fn write_counted(
    writer: &mut dyn InputWriter,
    bytes: &[u8],
    stall: Option<Duration>,
) -> (usize, std::io::Result<()>) {
    let mut accepted = 0;
    while accepted < bytes.len() {
        match writer.write_within(&bytes[accepted..], stall) {
            Ok(0) => {
                return (
                    accepted,
                    Err(std::io::Error::from(std::io::ErrorKind::WriteZero)),
                );
            }
            Ok(count) => accepted += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return (accepted, Err(error)),
        }
    }
    (accepted, Ok(()))
}

#[derive(Debug)]
enum Action {
    Join(watch::Receiver<Option<Result<(), StoredError>>>),
    Execute(watch::Sender<Option<Result<(), StoredError>>>),
}

#[derive(Debug)]
struct DedupState {
    entries: HashMap<String, DedupEntry>,
    completed: VecDeque<String>,
    namespace_id: Option<String>,
    mode: Option<DedupMode>,
    high_watermark: u64,
    legacy_evicted: EvictedFilter,
    capacity: usize,
}

impl DedupState {
    fn select_namespace(&mut self, namespace_id: &str, mode: DedupMode) {
        if self.namespace_id.as_deref() == Some(namespace_id) && self.mode == Some(mode) {
            return;
        }
        self.entries.clear();
        self.completed.clear();
        self.namespace_id = Some(namespace_id.to_owned());
        self.mode = Some(mode);
        self.high_watermark = 0;
        self.legacy_evicted.clear();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DedupMode {
    Monotonic,
    Legacy,
}

#[derive(Debug)]
struct DedupEntry {
    fingerprint: [u8; 32],
    result: watch::Receiver<Option<Result<(), StoredError>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StoredError {
    Io(String),
    WriterTask,
    Stalled {
        written: usize,
        total: usize,
        stall: Duration,
    },
}

impl StoredError {
    fn from_input(error: &InputError) -> Self {
        match error {
            InputError::Io(source) => Self::Io(source.to_string()),
            InputError::WriterTask => Self::WriterTask,
            InputError::Stalled {
                written,
                total,
                stall,
            } => Self::Stalled {
                written: *written,
                total: *total,
                stall: *stall,
            },
            InputError::InvalidCapacity
            | InputError::Conflict { .. }
            | InputError::OutcomeUnknown { .. } => Self::Io(error.to_string()),
        }
    }

    fn into_input(self) -> InputError {
        match self {
            Self::Io(message) => InputError::Io(std::io::Error::other(message)),
            Self::WriterTask => InputError::WriterTask,
            Self::Stalled {
                written,
                total,
                stall,
            } => InputError::Stalled {
                written,
                total,
                stall,
            },
        }
    }
}

async fn wait_result(
    mut receiver: watch::Receiver<Option<Result<(), StoredError>>>,
) -> Result<(), InputError> {
    loop {
        if let Some(result) = receiver.borrow().clone() {
            return result.map_err(StoredError::into_input);
        }
        receiver
            .changed()
            .await
            .map_err(|_channel_closed| InputError::WriterTask)?;
    }
}

fn fingerprint(plan: &InputPlan) -> [u8; 32] {
    let mut digest = Sha256::new();
    for fragment in &plan.fragments {
        digest.update(
            u64::try_from(fragment.bytes.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        digest.update(&fragment.bytes);
        digest.update(fragment.delay_after.as_nanos().to_be_bytes());
    }
    digest.finalize().into()
}

#[derive(Clone, PartialEq, Eq)]
struct EvictedFilter {
    words: [u64; LEGACY_EVICTED_FILTER_WORDS],
}

impl Debug for EvictedFilter {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvictedFilter").finish_non_exhaustive()
    }
}

impl EvictedFilter {
    fn new() -> Self {
        Self {
            words: [0; LEGACY_EVICTED_FILTER_WORDS],
        }
    }

    fn clear(&mut self) {
        self.words.fill(0);
    }

    fn insert(&mut self, write_id: &str) {
        for index in filter_indices(write_id) {
            self.words[index / 64] |= 1_u64 << (index % 64);
        }
    }

    fn might_contain(&self, write_id: &str) -> bool {
        filter_indices(write_id)
            .into_iter()
            .all(|index| self.words[index / 64] & (1_u64 << (index % 64)) != 0)
    }
}

fn filter_indices(write_id: &str) -> [usize; 3] {
    let digest: [u8; 32] = Sha256::digest(write_id.as_bytes()).into();
    let bit_count = LEGACY_EVICTED_FILTER_WORDS * 64;
    [0, 8, 16].map(|offset| {
        let value = u64::from_be_bytes(
            digest[offset..offset + 8]
                .try_into()
                .expect("fixed SHA-256 slice has eight bytes"),
        );
        usize::try_from(value % u64::try_from(bit_count).expect("filter size fits u64"))
            .expect("modulo filter size fits usize")
    })
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::{InputError, InputFragment, InputPlan, InputWriter, WriteCoordinator};
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::PathBuf;
    use std::time::Duration;

    /// Adapts a plain writer whose writes never stall, such as a file.
    struct Unbounded<W>(W);

    impl<W: Write + Send> InputWriter for Unbounded<W> {
        fn write_within(&mut self, buf: &[u8], _stall: Option<Duration>) -> std::io::Result<usize> {
            self.0.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.0.flush()
        }
    }

    const PRODUCER: &str = "daemon-a";

    /// Creates an empty output file in a private fixture directory.
    ///
    /// The returned guard removes the directory when it drops, also when a test
    /// fails.
    fn output_file(tag: &str) -> (tempfile::TempDir, PathBuf, fs::File) {
        let guard = pohunek_test_support::tempdir().expect("create fixture directory");
        let path = guard.path().join(tag);
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .expect("create output file");
        (guard, path, file)
    }

    fn plan(write_id: &str, bytes: &[u8]) -> InputPlan {
        InputPlan {
            write_id: write_id.to_owned(),
            fragments: vec![InputFragment {
                bytes: bytes.to_vec(),
                delay_after: Duration::ZERO,
            }],
        }
    }

    #[tokio::test]
    async fn ordered_fragments_are_written_once() {
        let (_guard, path, writer) = output_file("ordered");
        let coordinator = WriteCoordinator::new(Unbounded(writer), 8, None).expect("coordinator");
        let operation = InputPlan {
            write_id: "write-1".to_owned(),
            fragments: vec![
                InputFragment {
                    bytes: b"first".to_vec(),
                    delay_after: Duration::from_millis(1),
                },
                InputFragment {
                    bytes: b"-second".to_vec(),
                    delay_after: Duration::ZERO,
                },
            ],
        };

        coordinator
            .execute_control(PRODUCER, 1, operation.clone())
            .await
            .expect("first");
        coordinator
            .execute_control(PRODUCER, 1, operation)
            .await
            .expect("duplicate");

        assert_eq!(fs::read(&path).expect("read"), b"first-second");
    }

    #[tokio::test]
    async fn concurrent_duplicate_joins_same_result() {
        let (_guard, path, writer) = output_file("concurrent");
        let coordinator = WriteCoordinator::new(Unbounded(writer), 8, None).expect("coordinator");
        let operation = InputPlan {
            write_id: "write-1".to_owned(),
            fragments: vec![InputFragment {
                bytes: b"once".to_vec(),
                delay_after: Duration::from_millis(10),
            }],
        };

        let (first, second) = tokio::join!(
            coordinator.execute_control(PRODUCER, 1, operation.clone()),
            coordinator.execute_control(PRODUCER, 1, operation)
        );
        first.expect("first");
        second.expect("second");

        assert_eq!(fs::read(&path).expect("read"), b"once");
    }

    #[tokio::test]
    async fn conflicting_duplicate_is_rejected_without_writing() {
        let (_guard, path, writer) = output_file("conflict");
        let coordinator = WriteCoordinator::new(Unbounded(writer), 8, None).expect("coordinator");
        coordinator
            .execute_control(PRODUCER, 1, plan("write-1", b"a"))
            .await
            .expect("first");

        assert!(matches!(
            coordinator
                .execute_control(PRODUCER, 1, plan("write-1", b"b"))
                .await,
            Err(InputError::Conflict { .. })
        ));
        assert_eq!(fs::read(&path).expect("read"), b"a");
    }

    #[tokio::test]
    async fn evicted_write_returns_unknown_instead_of_replaying() {
        let (_guard, path, writer) = output_file("evicted");
        let coordinator = WriteCoordinator::new(Unbounded(writer), 1, None).expect("coordinator");
        coordinator
            .execute_control(PRODUCER, 1, plan("write-1", b"a"))
            .await
            .expect("first");
        coordinator
            .execute_control(PRODUCER, 2, plan("write-2", b"b"))
            .await
            .expect("second");

        assert!(matches!(
            coordinator
                .execute_control(PRODUCER, 1, plan("write-1", b"a"))
                .await,
            Err(InputError::OutcomeUnknown { .. })
        ));
        assert_eq!(fs::read(&path).expect("read"), b"ab");
    }

    #[tokio::test]
    async fn same_daemon_new_lease_restarts_sequence_in_fresh_namespace() {
        let (_guard, path, writer) = output_file("lease-rotation");
        let coordinator = WriteCoordinator::new(Unbounded(writer), 1, None).expect("coordinator");
        coordinator
            .execute_control("lease-a", 1, plan("input-lease-a-1", b"a"))
            .await
            .expect("first lease");
        coordinator
            .execute_control("lease-b", 1, plan("input-lease-b-1", b"b"))
            .await
            .expect("same daemon replacement lease");

        assert_eq!(fs::read(&path).expect("read"), b"ab");
    }

    #[tokio::test]
    async fn legacy_fresh_ids_may_arrive_out_of_order() {
        let (_guard, path, writer) = output_file("legacy-out-of-order");
        let coordinator = WriteCoordinator::new(Unbounded(writer), 8, None).expect("coordinator");
        coordinator
            .execute_legacy(PRODUCER, plan("input-2", b"two"))
            .await
            .expect("higher legacy id");
        coordinator
            .execute_legacy(PRODUCER, plan("input-1", b"one"))
            .await
            .expect("lower fresh legacy id");

        assert_eq!(fs::read(&path).expect("read"), b"twoone");
    }

    #[tokio::test]
    async fn ordered_attach_input_exceeds_control_dedup_capacity() {
        const ATTACH_INPUTS: usize = 8_192;

        let (_guard, path, writer) = output_file("attach-volume");
        let coordinator = WriteCoordinator::new(Unbounded(writer), 1, None).expect("coordinator");
        for row in 0..ATTACH_INPUTS {
            let report = format!("\x1b[<35;1;{}M", row % 80 + 1);
            coordinator
                .execute_stream(vec![InputFragment {
                    bytes: report.into_bytes(),
                    delay_after: Duration::ZERO,
                }])
                .await
                .expect("ordered attach input");
        }

        assert!(
            fs::metadata(&path).expect("metadata").len()
                > u64::try_from(ATTACH_INPUTS).expect("input count fits u64")
        );
    }

    #[test]
    fn debug_redacts_input_bytes() {
        let secret = b"seeded-secret-input";
        let rendered = format!("{:?}", plan("write-1", secret));

        assert!(rendered.contains("payload_bytes"));
        assert!(!rendered.contains("seeded-secret-input"));
    }
}
