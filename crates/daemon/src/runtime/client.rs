//! Private client for one durable session worker.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pohunek_worker_protocol::{
    read_frame, write_frame, AttachStart, Capability, ControlCode, ControlMessage, ControlReader,
    ControlRequest, ControlResponse, ControlWriter, DaemonId, DataFrame, Dimensions, ExitStatus,
    FrameHeader, FrameKind, Initialize, InputFragment, InputPlan, InspectSnapshot, LeaseId,
    OutputGap, RequestId, RequestKind, ResizeRequest, ResponseKind, RuntimeId, RuntimeScope,
    SessionId, StopRequest, StreamId, StreamMode, TransactionId, Version, WorkerId, WriteId,
    ATTACH_SNAPSHOT_VERSION, SUPPORTED_RANGE,
};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, OwnedMutexGuard};

// Rust guideline compliant 2026-08-31

/// Prefix for daemon-generated, producer-scoped control input identifiers.
const INPUT_WRITE_ID_PREFIX: &str = "input";
/// Maximum wait for the worker's attach readiness frame.
///
/// The frame follows a redeemed one-use token and requires no user input, so a
/// healthy local worker should respond promptly. Bounding it prevents an
/// unresponsive worker from indefinitely retaining the dimension-order gate.
const ATTACH_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Errors raised while controlling a durable worker.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    /// The worker socket could not be reached.
    #[error("worker socket operation failed at {path}: {source}")]
    Socket {
        /// Worker socket path.
        path: PathBuf,
        /// Underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
    /// A private control frame was malformed or unavailable.
    #[error("worker control protocol failed: {0}")]
    Protocol(String),
    /// The worker rejected an operation.
    #[error("worker rejected operation with {code:?}: {message}")]
    Rejected {
        /// Stable rejection code.
        code: ControlCode,
        /// Sanitized explanation.
        message: String,
        /// Whether the same request may succeed later.
        retryable: bool,
    },
    /// A response did not match the current request.
    #[error("worker response did not match the outstanding request")]
    ResponseMismatch,
    /// The worker runtime has not been initialized.
    #[error("worker has no live runtime generation")]
    NotInitialized,
    /// The worker cannot provide an atomic attach snapshot.
    #[error("worker protocol {selected_version} does not support atomic attach snapshots")]
    AttachSnapshotUnsupported {
        /// Protocol version selected for this daemon-worker connection.
        selected_version: Version,
    },
    /// The worker did not confirm the atomic attach start in time.
    #[error("worker attach readiness confirmation timed out after {timeout:?}")]
    AttachReadyTimeout {
        /// Maximum time allowed for the readiness frame.
        timeout: Duration,
    },
    /// The negotiated worker protocol cannot serve terminal observation.
    #[error("worker protocol {selected_version} does not support control-plane observation")]
    ObservationUnsupported {
        /// Protocol version selected for this daemon-worker connection.
        selected_version: Version,
    },
}

/// Runtime-bound output page returned by a private worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedOutput {
    /// Runtime that produced the output.
    pub runtime_id: RuntimeId,
    /// First currently retained byte offset.
    pub history_start_offset: u64,
    /// Offset of the first returned byte.
    pub start_offset: u64,
    /// Offset immediately after returned bytes.
    pub next_offset: u64,
    /// Current output end in this runtime.
    pub runtime_end_offset: u64,
    /// Opaque PTY bytes. Callers must not log these bytes.
    pub data: pohunek_worker_protocol::SecretBytes,
    /// Missing output range, when the requested cursor was evicted.
    pub gap: Option<OutputGap>,
    /// More retained bytes are immediately available.
    pub has_more: bool,
    /// Waiting reached the requested deadline.
    pub timed_out: bool,
}

/// Cloneable controller for one worker lease.
#[derive(Debug, Clone)]
pub struct Worker {
    inner: Arc<Mutex<Inner>>,
    socket_path: PathBuf,
    request_sequence: Arc<AtomicU64>,
    dimension_order: Arc<Mutex<()>>,
}

/// Prepared exclusive input write that has not started transport delivery.
#[derive(Debug)]
pub(crate) struct WriteReservation {
    inner: OwnedMutexGuard<Inner>,
    request: ControlRequest,
}

#[derive(Debug)]
struct Inner {
    reader: ControlReader<OwnedReadHalf>,
    writer: ControlWriter<OwnedWriteHalf>,
    selected_version: Version,
    session_id: SessionId,
    worker_id: WorkerId,
    runtime_id: Option<RuntimeId>,
    lease_id: LeaseId,
    capabilities: Vec<Capability>,
    next_write_sequence: u64,
}

/// One authenticated framed worker data stream.
#[derive(Debug)]
pub struct DataStream {
    /// Connected worker socket after the open frame.
    pub stream: UnixStream,
    /// Negotiated private protocol version.
    pub version: Version,
    /// Stream identity.
    pub stream_id: StreamId,
    /// Runtime generation.
    pub runtime_id: RuntimeId,
    /// Serialized dimension update held until the registry records it.
    pub dimension_update: Option<DimensionUpdate>,
}

/// Holds the worker's dimension-order gate until control-plane metadata commits.
#[derive(Debug)]
pub struct DimensionUpdate {
    dimensions: Dimensions,
    _order: OwnedMutexGuard<()>,
}

impl DimensionUpdate {
    /// Returns the authoritative PTY dimensions for this ordered update.
    #[must_use]
    pub const fn dimensions(&self) -> Dimensions {
        self.dimensions
    }
}

impl Worker {
    /// Returns the fixed owner-private control socket.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Connects, negotiates, and acquires the sole controller lease.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError`] when the socket, peer protocol, identity, or
    /// controller lease is unavailable.
    pub async fn connect(
        socket_path: impl AsRef<Path>,
        expected_session_id: &str,
        daemon_instance_id: &str,
    ) -> Result<Self, WorkerError> {
        let worker = Self::connect_discovered(socket_path, daemon_instance_id).await?;
        if worker.session_id().await.as_str() != expected_session_id {
            return Err(WorkerError::ResponseMismatch);
        }
        Ok(worker)
    }

    /// Connects to an owner-private endpoint and authenticates its claimed identity.
    ///
    /// This is restricted to startup discovery because the directory name is
    /// intentionally not trusted as the worker's logical session identity.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError`] when negotiation or controller acquisition fails.
    pub async fn connect_discovered(
        socket_path: impl AsRef<Path>,
        daemon_instance_id: &str,
    ) -> Result<Self, WorkerError> {
        Self::connect_discovered_with_range(
            socket_path,
            daemon_instance_id,
            SUPPORTED_RANGE.minimum(),
            SUPPORTED_RANGE.maximum(),
        )
        .await
    }

    /// Connects with an explicit compatibility range for upgrade/rollback tests.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError`] when the peer cannot negotiate the requested
    /// release range or its authenticated session identity differs.
    pub async fn connect_with_range(
        socket_path: impl AsRef<Path>,
        expected_session_id: &str,
        daemon_instance_id: &str,
        minimum_version: Version,
        maximum_version: Version,
    ) -> Result<Self, WorkerError> {
        let worker = Self::connect_discovered_with_range(
            socket_path,
            daemon_instance_id,
            minimum_version,
            maximum_version,
        )
        .await?;
        if worker.session_id().await.as_str() != expected_session_id {
            return Err(WorkerError::ResponseMismatch);
        }
        Ok(worker)
    }

    async fn connect_discovered_with_range(
        socket_path: impl AsRef<Path>,
        daemon_instance_id: &str,
        minimum_version: Version,
        maximum_version: Version,
    ) -> Result<Self, WorkerError> {
        let socket_path = socket_path.as_ref().to_path_buf();
        let stream =
            UnixStream::connect(&socket_path)
                .await
                .map_err(|source| WorkerError::Socket {
                    path: socket_path.clone(),
                    source,
                })?;
        let (read_half, write_half) = stream.into_split();
        let mut reader = ControlReader::new(read_half);
        let mut writer = ControlWriter::new(write_half);
        let daemon_id = DaemonId::new(daemon_instance_id)
            .map_err(|error| WorkerError::Protocol(error.to_string()))?;
        let request_id = RequestId::new("connect-negotiate")
            .map_err(|error| WorkerError::Protocol(error.to_string()))?;
        let response = exchange(
            &mut reader,
            &mut writer,
            ControlRequest {
                request_id,
                kind: RequestKind::Negotiate {
                    daemon_instance_id: daemon_id.clone(),
                    minimum_version,
                    maximum_version,
                },
            },
        )
        .await?;
        let (selected_version, session_id, worker_id, runtime_id, challenge, capabilities) =
            match response.kind {
                ResponseKind::Negotiated {
                    selected_version,
                    session_id,
                    worker_id,
                    runtime_id,
                    challenge,
                    capabilities,
                    ..
                } => (
                    selected_version,
                    session_id,
                    worker_id,
                    runtime_id,
                    challenge,
                    capabilities,
                ),
                other => return response_error(other),
            };
        let request_id = RequestId::new("connect-acquire")
            .map_err(|error| WorkerError::Protocol(error.to_string()))?;
        let requested_capabilities = requested_capabilities(selected_version, &capabilities);
        let response = exchange(
            &mut reader,
            &mut writer,
            ControlRequest {
                request_id,
                kind: RequestKind::AcquireController {
                    daemon_instance_id: daemon_id.clone(),
                    challenge,
                    requested_capabilities,
                },
            },
        )
        .await?;
        let (lease_id, capabilities) = match response.kind {
            ResponseKind::ControllerAcquired {
                lease_id,
                capabilities,
            } => (lease_id, capabilities),
            other => return response_error(other),
        };

        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                reader,
                writer,
                selected_version,
                session_id,
                worker_id,
                runtime_id,
                lease_id,
                capabilities,
                next_write_sequence: 1,
            })),
            socket_path,
            request_sequence: Arc::new(AtomicU64::new(1)),
            dimension_order: Arc::new(Mutex::new(())),
        })
    }

    /// Returns the authenticated logical session identity.
    pub async fn session_id(&self) -> SessionId {
        self.inner.lock().await.session_id.clone()
    }

    /// Returns the stable worker identity.
    pub async fn worker_id(&self) -> WorkerId {
        self.inner.lock().await.worker_id.clone()
    }

    /// Returns the current runtime identity.
    pub async fn runtime_id(&self) -> Option<RuntimeId> {
        self.inner.lock().await.runtime_id.clone()
    }

    /// Returns the worker's authoritative runtime snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError`] for connection or worker rejection.
    pub async fn inspect(&self) -> Result<InspectSnapshot, WorkerError> {
        let mut inner = self.inner.lock().await;
        let lease_id = inner.lease_id.clone();
        let response = request_locked(
            &mut inner,
            self.next_request_id("inspect")?,
            RequestKind::Inspect { lease_id },
        )
        .await?;
        match response {
            ResponseKind::Inspected { snapshot } => {
                inner.runtime_id.clone_from(&snapshot.runtime_id);
                Ok(*snapshot)
            }
            other => response_error(other),
        }
    }

    /// Initializes an uninitialized worker exactly once.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError`] for connection, identity, spawn, or journal
    /// failure.
    pub async fn initialize(&self, initialize: Initialize) -> Result<RuntimeId, WorkerError> {
        let mut inner = self.inner.lock().await;
        let lease_id = inner.lease_id.clone();
        let response = request_locked(
            &mut inner,
            self.next_request_id("initialize")?,
            RequestKind::Initialize {
                lease_id,
                initialize,
            },
        )
        .await?;
        match response {
            ResponseKind::Initialized { runtime_id, .. } => {
                inner.runtime_id = Some(runtime_id.clone());
                Ok(runtime_id)
            }
            other => response_error(other),
        }
    }

    /// Returns the current rendered terminal without creating an attach stream.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError::ObservationUnsupported`] for a previous worker
    /// and [`WorkerError`] when runtime identity or transport validation fails.
    pub async fn terminal_snapshot(
        &self,
    ) -> Result<pohunek_worker_protocol::TerminalSnapshot, WorkerError> {
        let mut inner = self.inner.lock().await;
        ensure_observation(&inner)?;
        let scope = scope(&inner)?;
        let response = request_locked(
            &mut inner,
            self.next_request_id("terminal-snapshot")?,
            RequestKind::TerminalSnapshot { scope },
        )
        .await?;
        match response {
            ResponseKind::TerminalSnapshot {
                runtime_id,
                snapshot,
            } if inner.runtime_id.as_ref() == Some(&runtime_id) => Ok(*snapshot),
            ResponseKind::TerminalSnapshot { .. } => Err(WorkerError::ResponseMismatch),
            other => response_error(other),
        }
    }

    /// Returns one runtime-bound page of retained output.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError::ObservationUnsupported`] for a previous worker
    /// and [`WorkerError`] when the worker rejects the cursor or byte bound.
    pub async fn read_output(
        &self,
        after_offset: Option<u64>,
        max_bytes: u32,
        wait: Duration,
    ) -> Result<ObservedOutput, WorkerError> {
        // Token issuance uses the shared controller connection. Shield this
        // short exchange from caller cancellation so a late response is always
        // consumed and cannot desynchronize the next control request. The
        // potentially long wait runs only on the dedicated data socket below.
        let worker = self.clone();
        let open =
            tokio::spawn(
                async move { worker.open_observation(after_offset, max_bytes, wait).await },
            )
            .await
            .map_err(|error| {
                WorkerError::Protocol(format!("observation opener task failed: {error}"))
            })??;
        read_observation_stream(&self.socket_path, open).await
    }

    async fn open_observation(
        &self,
        after_offset: Option<u64>,
        max_bytes: u32,
        wait: Duration,
    ) -> Result<ObservationOpen, WorkerError> {
        let mut inner = self.inner.lock().await;
        ensure_observation(&inner)?;
        let scope = scope(&inner)?;
        let wait_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX);
        let stream_id = self.next_stream_id("observation")?;
        let response = request_locked(
            &mut inner,
            self.next_request_id("read-output")?,
            RequestKind::ReadOutput {
                scope: scope.clone(),
                stream_id: stream_id.clone(),
                after_offset,
                max_bytes,
                wait_ms,
            },
        )
        .await?;
        match response {
            ResponseKind::OutputReadOpened { token, .. } => Ok(ObservationOpen {
                token,
                version: inner.selected_version,
                stream_id,
                runtime_id: scope.runtime_id,
                after_offset,
                max_bytes,
            }),
            other => response_error(other),
        }
    }

    /// Executes one lease-scoped deduplicated input plan.
    ///
    /// The sequence is allocated while the control mutex is held, so one daemon
    /// lease reaches the worker monotonically. Reconnecting acquires a new lease
    /// and therefore starts a new sequence namespace. This client intentionally
    /// does not retry an ambiguous exchange: it has no private reconnect path,
    /// and the public input request has no stable idempotency key. A future
    /// retry implementation must retain and resend the same generated plan
    /// rather than invoke this method again.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError`] for unavailable runtime or worker rejection.
    pub async fn write(&self, fragments: Vec<InputFragment>) -> Result<u64, WorkerError> {
        self.reserve_write(fragments).await?.commit(|| {}).await
    }

    /// Reserves the worker control connection and prepares one input plan.
    ///
    /// The returned reservation owns the exclusive control mutex. Dropping it
    /// before [`WriteReservation::commit`] writes no control bytes.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError`] for unavailable runtime or invalid plan framing.
    pub(crate) async fn reserve_write(
        &self,
        fragments: Vec<InputFragment>,
    ) -> Result<WriteReservation, WorkerError> {
        let mut inner = Arc::clone(&self.inner).lock_owned().await;
        let scope = scope(&inner)?;
        let sequence = inner.next_write_sequence;
        inner.next_write_sequence = sequence.checked_add(1).ok_or_else(|| {
            WorkerError::Protocol("worker input sequence was exhausted".to_owned())
        })?;
        let write_id = input_write_id(&inner.lease_id, sequence)?;
        let plan = InputPlan::new(write_id, fragments)
            .map_err(|error| WorkerError::Protocol(error.to_string()))?;
        Ok(WriteReservation {
            inner,
            request: ControlRequest {
                request_id: self.next_request_id("write")?,
                kind: RequestKind::WritePlan { scope, plan },
            },
        })
    }

    /// Applies an ordered PTY resize.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError`] for unavailable runtime or worker rejection.
    pub async fn resize(
        &self,
        source_id: StreamId,
        sequence: u64,
        dimensions: Dimensions,
    ) -> Result<DimensionUpdate, WorkerError> {
        let order = Arc::clone(&self.dimension_order).lock_owned().await;
        let mut inner = self.inner.lock().await;
        let scope = scope(&inner)?;
        let response = request_locked(
            &mut inner,
            self.next_request_id("resize")?,
            RequestKind::Resize {
                scope,
                resize: ResizeRequest {
                    source_id,
                    sequence,
                    dimensions,
                },
            },
        )
        .await?;
        match response {
            ResponseKind::Resized { dimensions, .. } => Ok(DimensionUpdate {
                dimensions,
                _order: order,
            }),
            other => response_error(other),
        }
    }

    /// Stops the retained PTY process group idempotently.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError`] for unavailable runtime or worker rejection.
    pub async fn stop(
        &self,
        transaction_id: TransactionId,
    ) -> Result<Option<ExitStatus>, WorkerError> {
        let mut inner = self.inner.lock().await;
        let scope = scope(&inner)?;
        let response = request_locked(
            &mut inner,
            self.next_request_id("stop")?,
            RequestKind::Stop {
                scope,
                stop: StopRequest { transaction_id },
            },
        )
        .await?;
        match response {
            ResponseKind::Stopped { exit } => Ok(exit),
            other => response_error(other),
        }
    }

    /// Opens an authenticated framed output/input stream.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError`] for unavailable runtime, token, or socket
    /// failure.
    pub async fn open_data(
        &self,
        stream_id: StreamId,
        mode: StreamMode,
        after_offset: Option<u64>,
    ) -> Result<DataStream, WorkerError> {
        self.open_data_with_attach(stream_id, mode, after_offset, None)
            .await
    }

    /// Opens a fresh public attachment with the best negotiated replay mode.
    ///
    /// Version-three workers preserve their established atomic terminal
    /// snapshot behavior. Version-four adds observation without changing the
    /// attach data stream:
    /// replacing a daemon must never make an otherwise live PTY unreachable.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError::AttachSnapshotUnsupported`] when a version-three
    /// worker did not grant its required snapshot capability, or another
    /// [`WorkerError`] when the worker rejects the stream or its socket fails.
    pub async fn open_attach(
        &self,
        stream_id: StreamId,
        attach: AttachStart,
    ) -> Result<DataStream, WorkerError> {
        let attach = self.attach_start(attach).await?;
        self.open_data_with_attach(stream_id, StreamMode::Attach, None, attach)
            .await
    }

    async fn attach_start(&self, attach: AttachStart) -> Result<Option<AttachStart>, WorkerError> {
        let inner = self.inner.lock().await;
        select_attach_start(inner.selected_version, &inner.capabilities, attach)
    }

    async fn open_data_with_attach(
        &self,
        stream_id: StreamId,
        mode: StreamMode,
        after_offset: Option<u64>,
        attach: Option<AttachStart>,
    ) -> Result<DataStream, WorkerError> {
        let dimension_order = if attach.is_some() {
            Some(Arc::clone(&self.dimension_order).lock_owned().await)
        } else {
            None
        };
        let mut inner = self.inner.lock().await;
        let scope = scope(&inner)?;
        let response = request_locked(
            &mut inner,
            self.next_request_id("open-data")?,
            RequestKind::OpenDataStream {
                scope: scope.clone(),
                stream_id: stream_id.clone(),
                mode,
                after_offset,
                attach: attach.clone(),
            },
        )
        .await?;
        let token = match response {
            ResponseKind::DataStreamOpened { token, .. } => token,
            other => return response_error(other),
        };
        let version = inner.selected_version;
        drop(inner);

        let mut stream = UnixStream::connect(&self.socket_path)
            .await
            .map_err(|source| WorkerError::Socket {
                path: self.socket_path.clone(),
                source,
            })?;
        let expects_attach_ready = attach.is_some();
        let open = DataFrame::new(
            FrameHeader {
                version,
                stream_id: stream_id.clone(),
                runtime_id: scope.runtime_id.clone(),
                kind: FrameKind::Open {
                    token,
                    mode,
                    after_offset,
                    attach,
                },
            },
            Vec::new(),
        )
        .map_err(|error| WorkerError::Protocol(error.to_string()))?;
        write_frame(&mut stream, &open)
            .await
            .map_err(|error| WorkerError::Protocol(error.to_string()))?;
        let dimension_update = if expects_attach_ready {
            Some(
                read_ordered_attach_ready(
                    &mut stream,
                    &stream_id,
                    &scope.runtime_id,
                    dimension_order.ok_or_else(|| {
                        WorkerError::Protocol(
                            "snapshot attach lost its dimension ordering guard".to_owned(),
                        )
                    })?,
                )
                .await?,
            )
        } else {
            None
        };
        Ok(DataStream {
            stream,
            version,
            stream_id,
            runtime_id: scope.runtime_id,
            dimension_update,
        })
    }

    /// Releases the exclusive controller lease before handing the worker to
    /// another daemon instance.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError`] when the worker rejects the current lease or the
    /// release response cannot be delivered.
    pub async fn release_controller(&self) -> Result<(), WorkerError> {
        let mut inner = self.inner.lock().await;
        let lease_id = inner.lease_id.clone();
        let response = request_locked(
            &mut inner,
            self.next_request_id("release-controller")?,
            RequestKind::ReleaseController { lease_id },
        )
        .await?;
        match response {
            ResponseKind::ControllerReleased => Ok(()),
            other => response_error(other),
        }
    }

    fn next_request_id(&self, operation: &str) -> Result<RequestId, WorkerError> {
        let sequence = self.request_sequence.fetch_add(1, Ordering::Relaxed);
        RequestId::new(format!("{operation}-{sequence}"))
            .map_err(|error| WorkerError::Protocol(error.to_string()))
    }

    fn next_stream_id(&self, operation: &str) -> Result<StreamId, WorkerError> {
        let sequence = self.request_sequence.fetch_add(1, Ordering::Relaxed);
        StreamId::new(format!("{operation}-{sequence}"))
            .map_err(|error| WorkerError::Protocol(error.to_string()))
    }
}

impl WriteReservation {
    /// Starts the prepared write and consumes its worker acknowledgement.
    ///
    /// `send_started` runs immediately before the first control writer await.
    /// Callers may cancel safely until that callback; after it, they must keep
    /// this future alive until the acknowledgement is consumed.
    pub(crate) async fn commit<F>(mut self, send_started: F) -> Result<u64, WorkerError>
    where
        F: FnOnce(),
    {
        let inner = &mut *self.inner;
        let response = exchange_marked(
            &mut inner.reader,
            &mut inner.writer,
            self.request,
            send_started,
        )
        .await?
        .kind;
        match response {
            ResponseKind::WriteCompleted { acknowledgement } => Ok(acknowledgement.bytes_written),
            other => response_error(other),
        }
    }
}

#[derive(Debug)]
struct ObservationOpen {
    token: pohunek_worker_protocol::DataToken,
    version: Version,
    stream_id: StreamId,
    runtime_id: RuntimeId,
    after_offset: Option<u64>,
    max_bytes: u32,
}

async fn read_observation_stream(
    socket_path: &Path,
    open: ObservationOpen,
) -> Result<ObservedOutput, WorkerError> {
    let mut stream =
        UnixStream::connect(socket_path)
            .await
            .map_err(|source| WorkerError::Socket {
                path: socket_path.to_path_buf(),
                source,
            })?;
    let open_frame = DataFrame::new(
        FrameHeader {
            version: open.version,
            stream_id: open.stream_id.clone(),
            runtime_id: open.runtime_id.clone(),
            kind: FrameKind::Open {
                token: open.token,
                mode: StreamMode::Observation,
                after_offset: open.after_offset,
                attach: None,
            },
        },
        Vec::new(),
    )
    .map_err(|error| WorkerError::Protocol(error.to_string()))?;
    write_frame(&mut stream, &open_frame)
        .await
        .map_err(|error| WorkerError::Protocol(error.to_string()))?;

    collect_observation_frames(
        &mut stream,
        open.version,
        open.stream_id,
        open.runtime_id,
        open.max_bytes,
    )
    .await
}

async fn collect_observation_frames(
    stream: &mut UnixStream,
    version: Version,
    stream_id: StreamId,
    runtime_id: RuntimeId,
    requested_max_bytes: u32,
) -> Result<ObservedOutput, WorkerError> {
    let start = read_matching_frame(stream, version, &stream_id, &runtime_id).await?;
    let (
        history_start_offset,
        start_offset,
        next_offset,
        runtime_end_offset,
        gap,
        has_more,
        timed_out,
    ) = match start.header().kind.clone() {
        FrameKind::ObservationStart {
            history_start_offset,
            start_offset,
            next_offset,
            runtime_end_offset,
            gap,
            has_more,
            timed_out,
        } => (
            history_start_offset,
            start_offset,
            next_offset,
            runtime_end_offset,
            gap,
            has_more,
            timed_out,
        ),
        FrameKind::Error { error } => {
            return Err(WorkerError::Rejected {
                code: error.code,
                message: error.message,
                retryable: error.retryable,
            });
        }
        _ => return Err(WorkerError::ResponseMismatch),
    };
    let declared_span = next_offset
        .checked_sub(start_offset)
        .ok_or(WorkerError::ResponseMismatch)?;
    if declared_span > u64::from(requested_max_bytes) {
        return Err(WorkerError::ResponseMismatch);
    }
    let requested_max_bytes = usize::try_from(requested_max_bytes)
        .map_err(|error| WorkerError::Protocol(error.to_string()))?;
    let expected_bytes =
        usize::try_from(declared_span).map_err(|error| WorkerError::Protocol(error.to_string()))?;
    let mut data = Vec::with_capacity(expected_bytes);
    let mut expected_offset = start_offset;
    loop {
        let frame = read_matching_frame(stream, version, &stream_id, &runtime_id).await?;
        match frame.header().kind.clone() {
            FrameKind::Replay { offset } if offset == expected_offset => {
                let accumulated = data
                    .len()
                    .checked_add(frame.payload().len())
                    .ok_or(WorkerError::ResponseMismatch)?;
                if accumulated > expected_bytes || accumulated > requested_max_bytes {
                    return Err(WorkerError::ResponseMismatch);
                }
                expected_offset = expected_offset
                    .checked_add(u64::try_from(frame.payload().len()).unwrap_or(u64::MAX))
                    .ok_or_else(|| {
                        WorkerError::Protocol("observation offset overflowed".to_owned())
                    })?;
                data.extend_from_slice(frame.payload());
            }
            FrameKind::Close {
                reason: pohunek_worker_protocol::CloseReason::ObservationComplete,
            } if expected_offset == next_offset && data.len() == expected_bytes => break,
            FrameKind::Error { error } => {
                return Err(WorkerError::Rejected {
                    code: error.code,
                    message: error.message,
                    retryable: error.retryable,
                });
            }
            _ => return Err(WorkerError::ResponseMismatch),
        }
    }
    Ok(ObservedOutput {
        runtime_id,
        history_start_offset,
        start_offset,
        next_offset,
        runtime_end_offset,
        data: pohunek_worker_protocol::SecretBytes::new(data),
        gap,
        has_more,
        timed_out,
    })
}

async fn read_matching_frame(
    stream: &mut UnixStream,
    version: Version,
    stream_id: &StreamId,
    runtime_id: &RuntimeId,
) -> Result<DataFrame, WorkerError> {
    let frame = read_frame(stream)
        .await
        .map_err(|error| WorkerError::Protocol(error.to_string()))?
        .ok_or_else(|| WorkerError::Protocol("observation stream closed early".to_owned()))?;
    if frame.header().version != version
        || frame.header().stream_id != *stream_id
        || frame.header().runtime_id != *runtime_id
    {
        return Err(WorkerError::ResponseMismatch);
    }
    Ok(frame)
}

fn requested_capabilities(selected_version: Version, advertised: &[Capability]) -> Vec<Capability> {
    [
        Capability::AtomicReplay,
        Capability::TerminalSnapshot,
        Capability::DeduplicatedInput,
        Capability::IdentityHook,
        Capability::AttachSnapshot,
        Capability::ControlPlaneObservation,
    ]
    .into_iter()
    .filter(|capability| {
        (*capability != Capability::AttachSnapshot || selected_version >= ATTACH_SNAPSHOT_VERSION)
            && (*capability != Capability::ControlPlaneObservation
                || selected_version >= pohunek_worker_protocol::CONTROL_PLANE_OBSERVATION_VERSION)
            && advertised.contains(capability)
    })
    .collect()
}

fn ensure_observation(inner: &Inner) -> Result<(), WorkerError> {
    if inner
        .capabilities
        .contains(&Capability::ControlPlaneObservation)
    {
        Ok(())
    } else {
        Err(WorkerError::ObservationUnsupported {
            selected_version: inner.selected_version,
        })
    }
}

fn supports_attach_snapshot(selected_version: Version, granted: &[Capability]) -> bool {
    selected_version >= ATTACH_SNAPSHOT_VERSION && granted.contains(&Capability::AttachSnapshot)
}

fn select_attach_start(
    selected_version: Version,
    granted: &[Capability],
    attach: AttachStart,
) -> Result<Option<AttachStart>, WorkerError> {
    if selected_version < ATTACH_SNAPSHOT_VERSION {
        return Ok(None);
    }
    if supports_attach_snapshot(selected_version, granted) {
        Ok(Some(attach))
    } else {
        Err(WorkerError::AttachSnapshotUnsupported { selected_version })
    }
}

fn scope(inner: &Inner) -> Result<RuntimeScope, WorkerError> {
    let runtime_id = inner
        .runtime_id
        .clone()
        .ok_or(WorkerError::NotInitialized)?;
    Ok(RuntimeScope {
        lease_id: inner.lease_id.clone(),
        session_id: inner.session_id.clone(),
        worker_id: inner.worker_id.clone(),
        runtime_id,
    })
}

async fn read_attach_ready(
    stream: &mut UnixStream,
    stream_id: &StreamId,
    runtime_id: &RuntimeId,
) -> Result<Dimensions, WorkerError> {
    read_attach_ready_with_timeout(stream, stream_id, runtime_id, ATTACH_READY_TIMEOUT).await
}

async fn read_attach_ready_with_timeout(
    stream: &mut UnixStream,
    stream_id: &StreamId,
    runtime_id: &RuntimeId,
    timeout: Duration,
) -> Result<Dimensions, WorkerError> {
    let frame = tokio::time::timeout(timeout, read_frame(stream))
        .await
        .map_err(|_elapsed| WorkerError::AttachReadyTimeout { timeout })?
        .map_err(|error| WorkerError::Protocol(error.to_string()))?
        .ok_or_else(|| {
            WorkerError::Protocol("worker attach closed before readiness confirmation".to_owned())
        })?;
    let (header, payload) = frame.into_parts();
    if header.stream_id != *stream_id || header.runtime_id != *runtime_id || !payload.is_empty() {
        return Err(WorkerError::Protocol(
            "worker attach readiness frame did not match its stream".to_owned(),
        ));
    }
    match header.kind {
        FrameKind::AttachReady { dimensions } => Ok(dimensions),
        FrameKind::Error { error } => Err(WorkerError::Rejected {
            code: error.code,
            message: error.message,
            retryable: error.retryable,
        }),
        _ => Err(WorkerError::Protocol(
            "worker attach did not begin with readiness confirmation".to_owned(),
        )),
    }
}

async fn read_ordered_attach_ready(
    stream: &mut UnixStream,
    stream_id: &StreamId,
    runtime_id: &RuntimeId,
    order: OwnedMutexGuard<()>,
) -> Result<DimensionUpdate, WorkerError> {
    let dimensions = read_attach_ready(stream, stream_id, runtime_id).await?;
    Ok(DimensionUpdate {
        dimensions,
        _order: order,
    })
}

fn input_write_id(lease_id: &LeaseId, sequence: u64) -> Result<WriteId, WorkerError> {
    WriteId::new(format!("{INPUT_WRITE_ID_PREFIX}-{lease_id}-{sequence}"))
        .map_err(|error| WorkerError::Protocol(error.to_string()))
}

async fn request_locked(
    inner: &mut Inner,
    request_id: RequestId,
    kind: RequestKind,
) -> Result<ResponseKind, WorkerError> {
    let response = exchange(
        &mut inner.reader,
        &mut inner.writer,
        ControlRequest { request_id, kind },
    )
    .await?;
    Ok(response.kind)
}

async fn exchange(
    reader: &mut ControlReader<OwnedReadHalf>,
    writer: &mut ControlWriter<OwnedWriteHalf>,
    request: ControlRequest,
) -> Result<ControlResponse, WorkerError> {
    exchange_marked(reader, writer, request, || {}).await
}

async fn exchange_marked<F>(
    reader: &mut ControlReader<OwnedReadHalf>,
    writer: &mut ControlWriter<OwnedWriteHalf>,
    request: ControlRequest,
    send_started: F,
) -> Result<ControlResponse, WorkerError>
where
    F: FnOnce(),
{
    let expected = request.request_id.clone();
    send_started();
    writer
        .write(&ControlMessage::Request(request))
        .await
        .map_err(|error| WorkerError::Protocol(error.to_string()))?;
    writer
        .flush()
        .await
        .map_err(|error| WorkerError::Protocol(error.to_string()))?;
    loop {
        let message = reader
            .read::<ControlMessage>()
            .await
            .map_err(|error| WorkerError::Protocol(error.to_string()))?
            .ok_or_else(|| WorkerError::Protocol("worker control connection closed".to_owned()))?;
        match message {
            ControlMessage::Response(response) if response.request_id == expected => {
                return Ok(response);
            }
            ControlMessage::Event(_) => {}
            ControlMessage::Response(_) | ControlMessage::Request(_) => {
                return Err(WorkerError::ResponseMismatch);
            }
        }
    }
}

fn response_error<T>(response: ResponseKind) -> Result<T, WorkerError> {
    match response {
        ResponseKind::Error { error } => Err(WorkerError::Rejected {
            code: error.code,
            message: error.message,
            retryable: error.retryable,
        }),
        _ => Err(WorkerError::ResponseMismatch),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::Ordering as AtomicOrdering;

    use pohunek_session_worker::{Server, ServerArgs, WorkerConfig};

    use super::*;

    static TEST_PATH_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    fn test_root(name: &str) -> PathBuf {
        let sequence = TEST_PATH_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
        std::env::temp_dir().join(format!("pohunek-{name}-{}-{sequence}", std::process::id()))
    }

    fn observation_initialize(root: &Path, output_bytes: usize) -> Initialize {
        Initialize {
            session_id: SessionId::new("s-9001").expect("session id"),
            transaction_id: TransactionId::new("transaction-e2e").expect("transaction id"),
            expected_worker_id: WorkerId::new("worker-e2e").expect("worker id"),
            launch: pohunek_worker_protocol::LaunchIdentity {
                agent: "test".to_owned(),
                agent_base: "test".to_owned(),
                reference_kind: None,
            },
            executable: PathBuf::from("/bin/sh"),
            arguments: vec![
                "-c".to_owned(),
                format!("/usr/bin/head -c {output_bytes} /dev/zero; sleep 1"),
            ],
            cwd: root.to_path_buf(),
            dimensions: Dimensions::new(80, 24).expect("dimensions"),
            environment: pohunek_worker_protocol::SecretEnv::new(BTreeMap::new())
                .expect("valid environment"),
            limits: pohunek_worker_protocol::InitializeLimits::new(
                u64::try_from(output_bytes).expect("history fits u64"),
                1_048_576,
                1_024,
                10_000,
            )
            .expect("initialize limits"),
            stop_policy: pohunek_worker_protocol::StopPolicy::new(500).expect("stop policy"),
            hook_protocol_version: pohunek_worker_protocol::CURRENT_VERSION,
            public_protocol_version: protocol::PROTOCOL_VERSION.get(),
        }
    }

    async fn write_observation_fixture(
        stream: &mut UnixStream,
        stream_id: &StreamId,
        runtime_id: &RuntimeId,
        payloads: &[&[u8]],
    ) {
        let byte_count = payloads.iter().map(|payload| payload.len()).sum::<usize>();
        let next_offset = u64::try_from(byte_count).expect("fixture length fits u64");
        let start = DataFrame::new(
            FrameHeader {
                version: pohunek_worker_protocol::CURRENT_VERSION,
                stream_id: stream_id.clone(),
                runtime_id: runtime_id.clone(),
                kind: FrameKind::ObservationStart {
                    history_start_offset: 0,
                    start_offset: 0,
                    next_offset,
                    runtime_end_offset: next_offset,
                    gap: None,
                    has_more: false,
                    timed_out: false,
                },
            },
            Vec::new(),
        )
        .expect("observation metadata");
        write_frame(stream, &start).await.expect("write metadata");
        let mut offset = 0_u64;
        for payload in payloads {
            let frame = DataFrame::new(
                FrameHeader {
                    version: pohunek_worker_protocol::CURRENT_VERSION,
                    stream_id: stream_id.clone(),
                    runtime_id: runtime_id.clone(),
                    kind: FrameKind::Replay { offset },
                },
                payload.to_vec(),
            )
            .expect("observation payload");
            write_frame(stream, &frame).await.expect("write payload");
            offset += u64::try_from(payload.len()).expect("payload fits u64");
        }
        let close = DataFrame::new(
            FrameHeader {
                version: pohunek_worker_protocol::CURRENT_VERSION,
                stream_id: stream_id.clone(),
                runtime_id: runtime_id.clone(),
                kind: FrameKind::Close {
                    reason: pohunek_worker_protocol::CloseReason::ObservationComplete,
                },
            },
            Vec::new(),
        )
        .expect("observation close");
        write_frame(stream, &close).await.expect("write close");
    }

    #[test]
    fn previous_negotiation_preserves_attach_snapshot_but_not_observation() {
        let requested = requested_capabilities(
            pohunek_worker_protocol::PREVIOUS_VERSION,
            &[
                Capability::AttachSnapshot,
                Capability::AtomicReplay,
                Capability::ControlPlaneObservation,
            ],
        );

        assert_eq!(
            requested,
            vec![Capability::AtomicReplay, Capability::AttachSnapshot]
        );
    }

    #[test]
    fn current_negotiation_requests_control_plane_observation() {
        let requested = requested_capabilities(
            pohunek_worker_protocol::CURRENT_VERSION,
            &[Capability::ControlPlaneObservation],
        );
        assert_eq!(requested, vec![Capability::ControlPlaneObservation]);
    }

    #[tokio::test]
    async fn real_worker_observation_handshake_streams_exact_public_limit_in_multiple_frames() {
        const SESSION_ID: &str = "s-9001";
        let root = test_root("observation-e2e");
        let socket_path = root.join("socket/worker.sock");
        let output_bytes = protocol::MAX_SESSION_OUTPUT_BYTES;
        let server = Server::bind(ServerArgs {
            session_id: SESSION_ID.to_owned(),
            worker_id: "worker-e2e".to_owned(),
            socket_path: socket_path.clone(),
            journal_path: root.join("journal/worker.json"),
            daemon_socket_path: root.join("daemon.sock"),
            config: WorkerConfig {
                data_payload_bytes: 1_024,
                ..WorkerConfig::new()
            },
        })
        .await
        .expect("bind real worker");
        let server_task = tokio::spawn(server.serve());
        let worker = Worker::connect(&socket_path, SESSION_ID, "daemon-e2e")
            .await
            .expect("control handshake");
        let runtime_id = worker
            .initialize(observation_initialize(&root, output_bytes))
            .await
            .expect("initialize runtime");

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let snapshot = worker.inspect().await.expect("inspect runtime");
                if snapshot.phase == pohunek_worker_protocol::RuntimePhase::Exited
                    && snapshot.next_offset
                        == u64::try_from(output_bytes).expect("output limit fits u64")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("runtime output completion");

        let observed = worker
            .read_output(
                Some(0),
                u32::try_from(output_bytes).expect("public limit fits u32"),
                Duration::ZERO,
            )
            .await
            .expect("dedicated observation stream");
        assert_eq!(observed.runtime_id, runtime_id);
        assert_eq!(observed.data.expose().len(), output_bytes);
        assert_eq!(observed.start_offset, 0);
        assert_eq!(
            observed.next_offset,
            u64::try_from(output_bytes).expect("output limit fits u64")
        );
        assert!(!observed.has_more);
        assert!(!observed.timed_out);

        worker
            .release_controller()
            .await
            .expect("release controller");
        server_task.abort();
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "the scripted peer keeps the cancellation and response ordering visible end to end"
    )]
    async fn cancelled_observation_control_exchange_does_not_desynchronize_next_request() {
        let root = test_root("observation-cancel");
        std::fs::create_dir_all(&root).expect("create test root");
        let socket_path = root.join("worker.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind fake worker");
        let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
        let (respond_tx, respond_rx) = tokio::sync::oneshot::channel();
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept control");
            let (read_half, write_half) = stream.into_split();
            let mut reader = ControlReader::new(read_half);
            let mut writer = ControlWriter::new(write_half);
            let session_id = SessionId::new("session-cancel").expect("session id");
            let worker_id = WorkerId::new("worker-cancel").expect("worker id");
            let runtime_id = RuntimeId::new("runtime-cancel").expect("runtime id");
            let challenge = pohunek_worker_protocol::LeaseChallenge::new("challenge-cancel")
                .expect("challenge");

            let request = reader
                .read::<ControlMessage>()
                .await
                .expect("read negotiation")
                .expect("negotiation");
            let ControlMessage::Request(request) = request else {
                panic!("control request");
            };
            assert!(matches!(request.kind, RequestKind::Negotiate { .. }));
            writer
                .write(&ControlMessage::Response(ControlResponse {
                    request_id: request.request_id,
                    kind: ResponseKind::Negotiated {
                        selected_version: pohunek_worker_protocol::CURRENT_VERSION,
                        supported_range: pohunek_worker_protocol::SUPPORTED_RANGE,
                        session_id: session_id.clone(),
                        worker_id: worker_id.clone(),
                        runtime_id: Some(runtime_id.clone()),
                        worker_process: pohunek_worker_protocol::ProcessIdentity {
                            pid: std::process::id(),
                            start_identity: 1,
                        },
                        phase: pohunek_worker_protocol::RuntimePhase::Running,
                        capabilities: vec![Capability::ControlPlaneObservation],
                        challenge: challenge.clone(),
                    },
                }))
                .await
                .expect("write negotiation");
            writer.flush().await.expect("flush negotiation");

            let request = reader
                .read::<ControlMessage>()
                .await
                .expect("read acquire")
                .expect("acquire");
            let ControlMessage::Request(request) = request else {
                panic!("control request");
            };
            assert!(matches!(
                request.kind,
                RequestKind::AcquireController { .. }
            ));
            writer
                .write(&ControlMessage::Response(ControlResponse {
                    request_id: request.request_id,
                    kind: ResponseKind::ControllerAcquired {
                        lease_id: LeaseId::new("lease-cancel").expect("lease id"),
                        capabilities: vec![Capability::ControlPlaneObservation],
                    },
                }))
                .await
                .expect("write acquire");
            writer.flush().await.expect("flush acquire");

            let request = reader
                .read::<ControlMessage>()
                .await
                .expect("read output request")
                .expect("output request");
            let ControlMessage::Request(request) = request else {
                panic!("control request");
            };
            assert!(matches!(request.kind, RequestKind::ReadOutput { .. }));
            request_seen_tx.send(()).expect("signal request");
            respond_rx.await.expect("allow delayed response");
            writer
                .write(&ControlMessage::Response(ControlResponse {
                    request_id: request.request_id,
                    kind: ResponseKind::OutputReadOpened {
                        token: pohunek_worker_protocol::DataToken::new("unused-cancel-token")
                            .expect("token"),
                        expires_at_ms: 10_000,
                    },
                }))
                .await
                .expect("write delayed response");
            writer.flush().await.expect("flush delayed response");

            let request = reader
                .read::<ControlMessage>()
                .await
                .expect("read snapshot request")
                .expect("snapshot request");
            let ControlMessage::Request(request) = request else {
                panic!("control request");
            };
            assert!(matches!(request.kind, RequestKind::TerminalSnapshot { .. }));
            writer
                .write(&ControlMessage::Response(ControlResponse {
                    request_id: request.request_id,
                    kind: ResponseKind::TerminalSnapshot {
                        runtime_id,
                        snapshot: Box::new(pohunek_worker_protocol::TerminalSnapshot {
                            watermark: 0,
                            dimensions: Dimensions::new(80, 24).expect("dimensions"),
                            cursor: pohunek_worker_protocol::Cursor {
                                column: 0,
                                row: 0,
                                visible: true,
                            },
                            alternate_screen: false,
                            title: None,
                            progress: None,
                            visible_lines: Vec::new(),
                        }),
                    },
                }))
                .await
                .expect("write snapshot");
            writer.flush().await.expect("flush snapshot");
        });

        let worker = Worker::connect(&socket_path, "session-cancel", "daemon-cancel")
            .await
            .expect("connect fake worker");
        let cancelled_worker = worker.clone();
        let read_task = tokio::spawn(async move {
            cancelled_worker
                .read_output(Some(0), 64, Duration::from_secs(1))
                .await
        });
        request_seen_rx.await.expect("output request reached peer");
        read_task.abort();
        let _ = read_task.await;
        respond_tx.send(()).expect("release response");

        let snapshot = tokio::time::timeout(Duration::from_secs(1), worker.terminal_snapshot())
            .await
            .expect("next request deadline")
            .expect("next request remains synchronized");
        assert_eq!(snapshot.watermark, 0);
        server_task.await.expect("fake worker task");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn observation_client_reassembles_multiple_frames() {
        let (mut client, mut worker) = UnixStream::pair().expect("socket pair");
        let stream_id = StreamId::new("observation-client").expect("stream id");
        let runtime_id = RuntimeId::new("runtime-client").expect("runtime id");
        write_observation_fixture(
            &mut worker,
            &stream_id,
            &runtime_id,
            &[&b"first"[..], &b"-second"[..]],
        )
        .await;

        let output = collect_observation_frames(
            &mut client,
            pohunek_worker_protocol::CURRENT_VERSION,
            stream_id,
            runtime_id,
            64,
        )
        .await
        .expect("collect output");
        assert_eq!(output.data.expose(), b"first-second");
        assert_eq!(output.next_offset, 12);
    }

    #[tokio::test]
    async fn observation_client_rejects_runtime_mismatch_without_payload_leak() {
        let (mut client, mut worker) = UnixStream::pair().expect("socket pair");
        let stream_id = StreamId::new("observation-client").expect("stream id");
        let expected_runtime = RuntimeId::new("runtime-expected").expect("runtime id");
        let wrong_runtime = RuntimeId::new("runtime-wrong").expect("runtime id");
        write_observation_fixture(&mut worker, &stream_id, &wrong_runtime, &[&b"secret"[..]]).await;

        let error = collect_observation_frames(
            &mut client,
            pohunek_worker_protocol::CURRENT_VERSION,
            stream_id,
            expected_runtime,
            64,
        )
        .await
        .expect_err("runtime mismatch");
        assert!(matches!(error, WorkerError::ResponseMismatch));
        assert!(!error.to_string().contains("secret"));
    }

    #[tokio::test]
    async fn observation_client_rejects_oversized_declared_span_before_allocation() {
        let (mut client, mut worker) = UnixStream::pair().expect("socket pair");
        let stream_id = StreamId::new("observation-client").expect("stream id");
        let runtime_id = RuntimeId::new("runtime-client").expect("runtime id");
        let metadata = DataFrame::new(
            FrameHeader {
                version: pohunek_worker_protocol::CURRENT_VERSION,
                stream_id: stream_id.clone(),
                runtime_id: runtime_id.clone(),
                kind: FrameKind::ObservationStart {
                    history_start_offset: 0,
                    start_offset: 0,
                    next_offset: u64::MAX,
                    runtime_end_offset: u64::MAX,
                    gap: None,
                    has_more: false,
                    timed_out: false,
                },
            },
            Vec::new(),
        )
        .expect("valid malicious metadata");
        write_frame(&mut worker, &metadata)
            .await
            .expect("write metadata");

        let error = collect_observation_frames(
            &mut client,
            pohunek_worker_protocol::CURRENT_VERSION,
            stream_id,
            runtime_id,
            1_024,
        )
        .await
        .expect_err("declared span above request must fail");
        assert!(matches!(error, WorkerError::ResponseMismatch));
    }

    #[tokio::test]
    async fn observation_client_rejects_v3_v4_frame_header_mismatch() {
        let (mut client, mut worker) = UnixStream::pair().expect("socket pair");
        let stream_id = StreamId::new("observation-client").expect("stream id");
        let runtime_id = RuntimeId::new("runtime-client").expect("runtime id");
        write_observation_fixture(&mut worker, &stream_id, &runtime_id, &[&b"data"[..]]).await;

        let error = collect_observation_frames(
            &mut client,
            pohunek_worker_protocol::PREVIOUS_VERSION,
            stream_id,
            runtime_id,
            64,
        )
        .await
        .expect_err("negotiated v3 must reject a v4 frame");
        assert!(matches!(error, WorkerError::ResponseMismatch));
    }

    #[tokio::test]
    async fn observation_client_caps_cumulative_replay_before_copying() {
        let (mut client, mut worker) = UnixStream::pair().expect("socket pair");
        let stream_id = StreamId::new("observation-client").expect("stream id");
        let runtime_id = RuntimeId::new("runtime-client").expect("runtime id");
        let metadata = DataFrame::new(
            FrameHeader {
                version: pohunek_worker_protocol::CURRENT_VERSION,
                stream_id: stream_id.clone(),
                runtime_id: runtime_id.clone(),
                kind: FrameKind::ObservationStart {
                    history_start_offset: 0,
                    start_offset: 0,
                    next_offset: 4,
                    runtime_end_offset: 4,
                    gap: None,
                    has_more: false,
                    timed_out: false,
                },
            },
            Vec::new(),
        )
        .expect("metadata");
        write_frame(&mut worker, &metadata)
            .await
            .expect("write metadata");
        let oversized = DataFrame::new(
            FrameHeader {
                version: pohunek_worker_protocol::CURRENT_VERSION,
                stream_id: stream_id.clone(),
                runtime_id: runtime_id.clone(),
                kind: FrameKind::Replay { offset: 0 },
            },
            b"12345".to_vec(),
        )
        .expect("replay frame");
        write_frame(&mut worker, &oversized)
            .await
            .expect("write replay");

        let error = collect_observation_frames(
            &mut client,
            pohunek_worker_protocol::CURRENT_VERSION,
            stream_id,
            runtime_id,
            4,
        )
        .await
        .expect_err("cumulative replay above declaration must fail");
        assert!(matches!(error, WorkerError::ResponseMismatch));
    }

    #[test]
    fn attach_snapshot_is_available_to_the_previous_protocol() {
        assert!(supports_attach_snapshot(
            pohunek_worker_protocol::PREVIOUS_VERSION,
            &[Capability::AttachSnapshot],
        ));
        assert!(!supports_attach_snapshot(ATTACH_SNAPSHOT_VERSION, &[]));
        assert!(supports_attach_snapshot(
            ATTACH_SNAPSHOT_VERSION,
            &[Capability::AttachSnapshot],
        ));
    }

    #[test]
    fn previous_attach_keeps_the_snapshot_request() {
        let attach = AttachStart { dimensions: None };
        assert_eq!(
            select_attach_start(
                pohunek_worker_protocol::PREVIOUS_VERSION,
                &[Capability::AttachSnapshot],
                attach.clone()
            )
            .expect("v3 snapshot capability must be supported"),
            Some(attach),
        );
    }

    #[test]
    fn v3_attach_keeps_the_snapshot_request() {
        let attach = AttachStart { dimensions: None };
        assert_eq!(
            select_attach_start(
                ATTACH_SNAPSHOT_VERSION,
                &[Capability::AttachSnapshot],
                attach.clone()
            )
            .expect("v3 snapshot capability must be supported"),
            Some(attach),
        );
    }

    #[test]
    fn v3_attach_without_snapshot_capability_is_rejected() {
        let error = select_attach_start(
            ATTACH_SNAPSHOT_VERSION,
            &[],
            AttachStart { dimensions: None },
        )
        .expect_err("v3 workers must not receive a v2 attach frame");
        assert!(matches!(
            error,
            WorkerError::AttachSnapshotUnsupported {
                selected_version: ATTACH_SNAPSHOT_VERSION
            }
        ));
    }

    #[tokio::test]
    async fn attach_ready_reports_authoritative_dimensions() {
        let (mut client, mut worker) = UnixStream::pair().expect("worker stream pair");
        let stream_id = StreamId::new("a-ready").expect("stream id");
        let runtime_id = RuntimeId::new("runtime-ready").expect("runtime id");
        let dimensions = Dimensions::new(100, 30).expect("dimensions");
        let frame = DataFrame::new(
            FrameHeader {
                version: pohunek_worker_protocol::CURRENT_VERSION,
                stream_id: stream_id.clone(),
                runtime_id: runtime_id.clone(),
                kind: FrameKind::AttachReady { dimensions },
            },
            Vec::new(),
        )
        .expect("ready frame");
        write_frame(&mut worker, &frame)
            .await
            .expect("write ready frame");

        assert_eq!(
            read_attach_ready(&mut client, &stream_id, &runtime_id)
                .await
                .expect("read ready frame"),
            dimensions
        );
    }

    #[tokio::test]
    async fn attach_ready_surfaces_the_worker_error() {
        let (mut client, mut worker) = UnixStream::pair().expect("worker stream pair");
        let stream_id = StreamId::new("a-error").expect("stream id");
        let runtime_id = RuntimeId::new("runtime-error").expect("runtime id");
        let frame = DataFrame::new(
            FrameHeader {
                version: pohunek_worker_protocol::CURRENT_VERSION,
                stream_id: stream_id.clone(),
                runtime_id: runtime_id.clone(),
                kind: FrameKind::Error {
                    error: pohunek_worker_protocol::ControlError {
                        code: ControlCode::InvalidState,
                        message: "runtime exited before attach completed".to_owned(),
                        retryable: false,
                    },
                },
            },
            Vec::new(),
        )
        .expect("error frame");
        write_frame(&mut worker, &frame)
            .await
            .expect("write error frame");

        assert!(matches!(
            read_attach_ready(&mut client, &stream_id, &runtime_id).await,
            Err(WorkerError::Rejected {
                code: ControlCode::InvalidState,
                retryable: false,
                ..
            })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn attach_ready_timeout_releases_dimension_order() {
        let (mut client, _worker) = UnixStream::pair().expect("worker stream pair");
        let stream_id = StreamId::new("a-timeout").expect("stream id");
        let runtime_id = RuntimeId::new("runtime-timeout").expect("runtime id");
        let order = Arc::new(Mutex::new(()));
        let guard = Arc::clone(&order).lock_owned().await;

        let ready = tokio::spawn(async move {
            read_ordered_attach_ready(&mut client, &stream_id, &runtime_id, guard).await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(ATTACH_READY_TIMEOUT).await;

        assert!(matches!(
            ready.await.expect("readiness task joins"),
            Err(WorkerError::AttachReadyTimeout {
                timeout: ATTACH_READY_TIMEOUT
            })
        ));
        assert!(
            order.try_lock().is_ok(),
            "a timeout must release the dimension-order gate"
        );
    }

    #[tokio::test]
    async fn dimension_update_holds_order_until_metadata_commit_finishes() {
        let order = Arc::new(Mutex::new(()));
        let guard = Arc::clone(&order).lock_owned().await;
        let update = DimensionUpdate {
            dimensions: Dimensions::new(100, 30).expect("dimensions"),
            _order: guard,
        };

        assert!(
            order.try_lock().is_err(),
            "a later resize must wait while the registry owns the update"
        );
        drop(update);
        assert!(
            order.try_lock().is_ok(),
            "the ordering gate must reopen after metadata commit"
        );
    }

    #[test]
    fn current_protocol_is_in_supported_range() {
        assert!(SUPPORTED_RANGE.contains(pohunek_worker_protocol::CURRENT_VERSION));
    }

    #[test]
    fn replacement_lease_restarts_input_sequence_in_a_fresh_namespace() {
        let first = LeaseId::new("lease-a").expect("first lease");
        let replacement = LeaseId::new("lease-b").expect("replacement lease");

        assert_eq!(
            input_write_id(&first, 1).expect("first input").as_str(),
            "input-lease-a-1"
        );
        assert_eq!(
            input_write_id(&replacement, 1)
                .expect("replacement input")
                .as_str(),
            "input-lease-b-1"
        );
    }
}
