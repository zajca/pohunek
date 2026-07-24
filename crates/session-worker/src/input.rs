//! Executes ordered PTY input plans with bounded deduplication.

// Rust guideline compliant 2026-07-23

use std::collections::{HashMap, VecDeque};
use std::fmt::{Debug, Formatter};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::sync::{watch, Mutex as AsyncMutex};

/// Number of bits retained for conservative evicted-write detection.
///
/// False positives return `OutcomeUnknown`, which is safe because the worker
/// does not duplicate input. The fixed memory prevents unbounded lifetime state.
const EVICTED_FILTER_WORDS: usize = 256;

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
    /// A previously evicted write may already have completed.
    #[error("write outcome is unknown for expired id `{write_id}`")]
    OutcomeUnknown {
        /// Ambiguous write ID.
        write_id: String,
    },
    /// The PTY writer failed.
    #[error("PTY input write failed: {0}")]
    Io(std::io::Error),
    /// A blocking writer task failed.
    #[error("PTY input writer task terminated unexpectedly")]
    WriterTask,
}

/// Cloneable ordered input coordinator.
#[derive(Clone)]
pub struct WriteCoordinator {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    execution: Arc<AsyncMutex<()>>,
    state: Arc<Mutex<DedupState>>,
}

impl Debug for WriteCoordinator {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let state = lock(&self.state);
        f.debug_struct("WriteCoordinator")
            .field("retained_entries", &state.entries.len())
            .field("capacity", &state.capacity)
            .finish_non_exhaustive()
    }
}

impl WriteCoordinator {
    /// Creates a coordinator around a real writer.
    ///
    /// # Errors
    ///
    /// Returns [`InputError::InvalidCapacity`] when `capacity` is zero.
    pub fn new(writer: impl Write + Send + 'static, capacity: usize) -> Result<Self, InputError> {
        if capacity == 0 {
            return Err(InputError::InvalidCapacity);
        }
        Ok(Self {
            writer: Arc::new(Mutex::new(Box::new(writer))),
            execution: Arc::new(AsyncMutex::new(())),
            state: Arc::new(Mutex::new(DedupState {
                entries: HashMap::new(),
                completed: VecDeque::new(),
                evicted: EvictedFilter::new(),
                capacity,
            })),
        })
    }

    /// Executes or joins an idempotent write plan.
    ///
    /// Completion is published only after all fragments are flushed. Plans are
    /// globally serialized so their delayed fragments cannot interleave.
    ///
    /// # Errors
    ///
    /// Returns [`InputError`] for conflicting, ambiguous, or failed writes.
    pub async fn execute(&self, plan: InputPlan) -> Result<(), InputError> {
        let fingerprint = fingerprint(&plan);
        let action = {
            let mut state = lock(&self.state);
            match state.entries.get(&plan.write_id) {
                Some(entry) if entry.fingerprint != fingerprint => {
                    return Err(InputError::Conflict {
                        write_id: plan.write_id,
                    });
                }
                Some(entry) => Action::Join(entry.result.clone()),
                None if state.evicted.might_contain(&plan.write_id) => {
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
                let result = self.execute_once(&plan).await;
                let stored = result.as_ref().copied().map_err(StoredError::from_input);
                self.complete(&plan.write_id);
                let _ = sender.send(Some(stored));
                result
            }
        }
    }

    async fn execute_once(&self, plan: &InputPlan) -> Result<(), InputError> {
        let _execution = self.execution.lock().await;
        for fragment in &plan.fragments {
            let bytes = fragment.bytes.clone();
            let writer = Arc::clone(&self.writer);
            tokio::task::spawn_blocking(move || {
                let mut writer = lock(&writer);
                writer.write_all(&bytes)?;
                writer.flush()
            })
            .await
            .map_err(|_join_error| InputError::WriterTask)?
            .map_err(InputError::Io)?;
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
            state.evicted.insert(&evicted_id);
        }
    }
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
    evicted: EvictedFilter,
    capacity: usize,
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
}

impl StoredError {
    fn from_input(error: &InputError) -> Self {
        match error {
            InputError::Io(source) => Self::Io(source.to_string()),
            InputError::WriterTask => Self::WriterTask,
            InputError::InvalidCapacity
            | InputError::Conflict { .. }
            | InputError::OutcomeUnknown { .. } => Self::Io(error.to_string()),
        }
    }

    fn into_input(self) -> InputError {
        match self {
            Self::Io(message) => InputError::Io(std::io::Error::other(message)),
            Self::WriterTask => InputError::WriterTask,
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
    words: [u64; EVICTED_FILTER_WORDS],
}

impl Debug for EvictedFilter {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvictedFilter").finish_non_exhaustive()
    }
}

impl EvictedFilter {
    fn new() -> Self {
        Self {
            words: [0; EVICTED_FILTER_WORDS],
        }
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
    let bit_count = EVICTED_FILTER_WORDS * 64;
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
    use super::{InputError, InputFragment, InputPlan, WriteCoordinator};
    use std::fs::{self, OpenOptions};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn output_file(tag: &str) -> (PathBuf, fs::File) {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pohunek-worker-input-{tag}-{}-{sequence}",
            std::process::id()
        ));
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .expect("create output file");
        (path, file)
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
        let (path, writer) = output_file("ordered");
        let coordinator = WriteCoordinator::new(writer, 8).expect("coordinator");
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

        coordinator.execute(operation.clone()).await.expect("first");
        coordinator.execute(operation).await.expect("duplicate");

        assert_eq!(fs::read(&path).expect("read"), b"first-second");
        fs::remove_file(path).expect("cleanup");
    }

    #[tokio::test]
    async fn concurrent_duplicate_joins_same_result() {
        let (path, writer) = output_file("concurrent");
        let coordinator = WriteCoordinator::new(writer, 8).expect("coordinator");
        let operation = InputPlan {
            write_id: "write-1".to_owned(),
            fragments: vec![InputFragment {
                bytes: b"once".to_vec(),
                delay_after: Duration::from_millis(10),
            }],
        };

        let (first, second) = tokio::join!(
            coordinator.execute(operation.clone()),
            coordinator.execute(operation)
        );
        first.expect("first");
        second.expect("second");

        assert_eq!(fs::read(&path).expect("read"), b"once");
        fs::remove_file(path).expect("cleanup");
    }

    #[tokio::test]
    async fn conflicting_duplicate_is_rejected_without_writing() {
        let (path, writer) = output_file("conflict");
        let coordinator = WriteCoordinator::new(writer, 8).expect("coordinator");
        coordinator
            .execute(plan("write-1", b"a"))
            .await
            .expect("first");

        assert!(matches!(
            coordinator.execute(plan("write-1", b"b")).await,
            Err(InputError::Conflict { .. })
        ));
        assert_eq!(fs::read(&path).expect("read"), b"a");
        fs::remove_file(path).expect("cleanup");
    }

    #[tokio::test]
    async fn evicted_write_returns_unknown_instead_of_replaying() {
        let (path, writer) = output_file("evicted");
        let coordinator = WriteCoordinator::new(writer, 1).expect("coordinator");
        coordinator
            .execute(plan("write-1", b"a"))
            .await
            .expect("first");
        coordinator
            .execute(plan("write-2", b"b"))
            .await
            .expect("second");

        assert!(matches!(
            coordinator.execute(plan("write-1", b"a")).await,
            Err(InputError::OutcomeUnknown { .. })
        ));
        assert_eq!(fs::read(&path).expect("read"), b"ab");
        fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn debug_redacts_input_bytes() {
        let secret = b"seeded-secret-input";
        let rendered = format!("{:?}", plan("write-1", secret));

        assert!(rendered.contains("payload_bytes"));
        assert!(!rendered.contains("seeded-secret-input"));
    }
}
