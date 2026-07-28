//! Private client for one durable session worker.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use pohunek_worker_protocol::{
    write_frame, Capability, ControlCode, ControlMessage, ControlReader, ControlRequest,
    ControlResponse, ControlWriter, DaemonId, DataFrame, Dimensions, ExitStatus, FrameHeader,
    FrameKind, Initialize, InputFragment, InputPlan, InspectSnapshot, LeaseId, RequestId,
    RequestKind, ResizeRequest, ResponseKind, RuntimeId, RuntimeScope, SessionId, StopRequest,
    StreamId, StreamMode, TransactionId, Version, WorkerId, WriteId, SUPPORTED_RANGE,
};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

// Rust guideline compliant 2026-07-28

/// Prefix for daemon-generated, producer-scoped control input identifiers.
const INPUT_WRITE_ID_PREFIX: &str = "input";

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
}

/// Cloneable controller for one worker lease.
#[derive(Debug, Clone)]
pub struct Worker {
    inner: Arc<Mutex<Inner>>,
    socket_path: PathBuf,
    request_sequence: Arc<AtomicU64>,
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
        let requested_capabilities = [
            Capability::AtomicReplay,
            Capability::TerminalSnapshot,
            Capability::DeduplicatedInput,
            Capability::IdentityHook,
        ]
        .into_iter()
        .filter(|capability| capabilities.contains(capability))
        .collect();
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
        let lease_id = match response.kind {
            ResponseKind::ControllerAcquired { lease_id, .. } => lease_id,
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
                next_write_sequence: 1,
            })),
            socket_path,
            request_sequence: Arc::new(AtomicU64::new(1)),
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
        let mut inner = self.inner.lock().await;
        let scope = scope(&inner)?;
        let sequence = inner.next_write_sequence;
        inner.next_write_sequence = sequence.checked_add(1).ok_or_else(|| {
            WorkerError::Protocol("worker input sequence was exhausted".to_owned())
        })?;
        let write_id = input_write_id(&inner.lease_id, sequence)?;
        let plan = InputPlan::new(write_id, fragments)
            .map_err(|error| WorkerError::Protocol(error.to_string()))?;
        let response = request_locked(
            &mut inner,
            self.next_request_id("write")?,
            RequestKind::WritePlan { scope, plan },
        )
        .await?;
        match response {
            ResponseKind::WriteCompleted { acknowledgement } => Ok(acknowledgement.bytes_written),
            other => response_error(other),
        }
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
    ) -> Result<Dimensions, WorkerError> {
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
            ResponseKind::Resized { dimensions, .. } => Ok(dimensions),
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
        let open = DataFrame::new(
            FrameHeader {
                version,
                stream_id: stream_id.clone(),
                runtime_id: scope.runtime_id.clone(),
                kind: FrameKind::Open {
                    token,
                    mode,
                    after_offset,
                },
            },
            Vec::new(),
        )
        .map_err(|error| WorkerError::Protocol(error.to_string()))?;
        write_frame(&mut stream, &open)
            .await
            .map_err(|error| WorkerError::Protocol(error.to_string()))?;
        Ok(DataStream {
            stream,
            version,
            stream_id,
            runtime_id: scope.runtime_id,
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
    let expected = request.request_id.clone();
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
    use super::*;

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
