//! Exercises controller replacement while an old release response is flushing.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use pohunek_test_support::wait::guard;
use pohunek_worker_protocol::{
    ControlMessage, ControlReader, ControlRequest, ControlWriter, DaemonId, LeaseId, RequestId,
    RequestKind, ResponseKind, CURRENT_VERSION,
};
use tokio::io::{AsyncWrite, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::oneshot;
use tokio::task::JoinSet;

use super::{environment_fixture, owner_peer, BarrierDirectory};
use crate::server::{serve_control, Shared};
use crate::{LeaseOwner, WorkerError};

// Enough space for control responses, so only the explicit flush gate blocks.
const CONTROL_BUFFER: usize = 16 * 1024;
// The gated connection answers Negotiate, AcquireController, then ReleaseController.
const RELEASE_RESPONSE_NUMBER: usize = 3;
const DAEMON_ID: &str = "daemon-controller-cleanup";

struct FlushGate {
    armed: Arc<AtomicBool>,
    entered: Option<oneshot::Sender<()>>,
    release: oneshot::Receiver<()>,
    waiting: bool,
}

struct GatedWriter {
    inner: WriteHalf<DuplexStream>,
    gate: Option<FlushGate>,
    responses_written: usize,
}

impl AsyncWrite for GatedWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(written)) = result {
            // ControlWriter writes one JSON response at a time; its final
            // byte is the only literal newline, including with partial writes.
            self.responses_written += usize::from(buf[..written].ends_with(b"\n"));
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let release_written = self.responses_written >= RELEASE_RESPONSE_NUMBER;
        if let Some(gate) = &mut self.gate {
            if release_written && gate.armed.swap(false, Ordering::SeqCst) {
                gate.waiting = true;
                if let Some(entered) = gate.entered.take() {
                    entered.send(()).expect("flush observer remains live");
                }
            }
            if gate.waiting {
                match Pin::new(&mut gate.release).poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(result) => {
                        result.expect("flush release remains live");
                        gate.waiting = false;
                    }
                }
            }
        }
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

struct Controller {
    reader: ControlReader<ReadHalf<DuplexStream>>,
    writer: ControlWriter<WriteHalf<DuplexStream>>,
}

impl Controller {
    async fn response(&mut self) -> ResponseKind {
        match self
            .reader
            .read::<ControlMessage>()
            .await
            .expect("decode control response")
            .expect("control connection remains live")
        {
            ControlMessage::Response(response) => response.kind,
            message => panic!("expected a control response, got {message:?}"),
        }
    }

    async fn request(&mut self, kind: RequestKind) -> ResponseKind {
        self.writer
            .write(&ControlMessage::Request(ControlRequest {
                request_id: RequestId::new("cleanup-request").expect("request id"),
                kind,
            }))
            .await
            .expect("write control request");
        self.writer.flush().await.expect("flush control request");
        self.response().await
    }
}

async fn acquire_controller(
    shared: Arc<Shared>,
    owner: &LeaseOwner,
    gate: Option<FlushGate>,
    tasks: &mut JoinSet<Result<(), WorkerError>>,
) -> (Controller, LeaseId) {
    let (client, server) = tokio::io::duplex(CONTROL_BUFFER);
    let (read_half, write_half) = tokio::io::split(server);
    let first = ControlRequest {
        request_id: RequestId::new("cleanup-negotiate").expect("request id"),
        kind: RequestKind::Negotiate {
            daemon_instance_id: DaemonId::new(DAEMON_ID).expect("daemon id"),
            minimum_version: CURRENT_VERSION,
            maximum_version: CURRENT_VERSION,
        },
    };
    let peer = Arc::new(owner_peer(owner));
    tasks.spawn(async move {
        serve_control(
            shared,
            &mut ControlReader::new(read_half),
            &mut ControlWriter::new(GatedWriter {
                inner: write_half,
                gate,
                responses_written: 0,
            }),
            first,
            peer,
        )
        .await
    });
    let (read_half, write_half) = tokio::io::split(client);
    let mut controller = Controller {
        reader: ControlReader::new(read_half),
        writer: ControlWriter::new(write_half),
    };
    let ResponseKind::Negotiated { challenge, .. } = controller.response().await else {
        panic!("controller negotiation must succeed");
    };
    let response = controller
        .request(RequestKind::AcquireController {
            daemon_instance_id: DaemonId::new(DAEMON_ID).expect("daemon id"),
            challenge,
            requested_capabilities: Vec::new(),
        })
        .await;
    let ResponseKind::ControllerAcquired { lease_id, .. } = response else {
        panic!("controller acquisition must succeed: {response:?}");
    };
    (controller, lease_id)
}

#[tokio::test]
async fn old_release_finalizer_preserves_the_replacement_controller() {
    guard(
        "controller replacement across the old release flush",
        async {
            let directory = BarrierDirectory::new();
            let server = environment_fixture(directory.path(), "abcd2345")
                .await
                .expect("bind worker fixture");
            let shared = Arc::clone(&server.shared);
            let owner = LeaseOwner {
                daemon_id: DAEMON_ID.to_owned(),
                peer_pid: std::process::id(),
                peer_start_identity: "7200".to_owned(),
            };
            let armed = Arc::new(AtomicBool::new(false));
            let (entered_tx, entered_rx) = oneshot::channel();
            let (release_tx, release_rx) = oneshot::channel();
            let mut tasks = JoinSet::new();
            let (mut old, old_lease) = acquire_controller(
                Arc::clone(&shared),
                &owner,
                Some(FlushGate {
                    armed: Arc::clone(&armed),
                    entered: Some(entered_tx),
                    release: release_rx,
                    waiting: false,
                }),
                &mut tasks,
            )
            .await;
            armed.store(true, Ordering::SeqCst);
            assert!(matches!(
                old.request(RequestKind::ReleaseController {
                    lease_id: old_lease.clone()
                })
                .await,
                ResponseKind::ControllerReleased
            ));
            entered_rx.await.expect("old release is parked at flush");
            assert!(!shared.lease.is_active());
            let released_epoch = *shared.lease_epoch_tx.borrow();

            let (mut replacement, replacement_lease) =
                acquire_controller(Arc::clone(&shared), &owner, None, &mut tasks).await;
            assert_ne!(old_lease, replacement_lease);
            release_tx.send(()).expect("resume old release finalizer");
            tasks
                .join_next()
                .await
                .expect("old controller task exists")
                .expect("old controller task joins")
                .expect("old controller closes successfully");
            let inspected = replacement
                .request(RequestKind::Inspect {
                    lease_id: replacement_lease,
                })
                .await;
            assert!(
                matches!(inspected, ResponseKind::Inspected { .. }),
                "replacement controller must remain usable: {inspected:?}"
            );
            assert_eq!(
                *shared.lease_epoch_tx.borrow(),
                released_epoch,
                "stale cleanup must not advance the replacement lease epoch"
            );

            drop(replacement);
            tasks
                .join_next()
                .await
                .expect("replacement controller task exists")
                .expect("replacement controller task joins")
                .expect("replacement controller closes successfully");
            assert!(!shared.lease.is_active());
            assert_eq!(*shared.lease_epoch_tx.borrow(), released_epoch + 1);
        },
    )
    .await;
}
