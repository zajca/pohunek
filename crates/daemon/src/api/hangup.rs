//! Detection of a client that closed its attach connection while the attach
//! bridge waits for the program.
//!
//! A bridge with a pending input send does not read its client, so it cannot
//! see an end of stream. A Unix socket whose peer closed completely (not just
//! its write half, as a piped client does after its stdin ended) reports a
//! hangup, which a duplicate descriptor can wait for without consuming input.
//! A TCP connection reports a peer's close only through reads and writes, so
//! its bridge has no such watch.

use std::future::Future;
use std::os::fd::AsFd;
use std::pin::Pin;

use tokio::io::unix::AsyncFd;
use tokio::io::{DuplexStream, Interest};
use tokio::net::{TcpStream, UnixStream};
use tracing::warn;

/// Resolves when the client's connection was closed completely.
pub(super) struct HangupWatch(Pin<Box<dyn Future<Output = ()> + Send>>);

impl HangupWatch {
    pub(super) async fn closed(&mut self) {
        self.0.as_mut().await;
    }
}

/// A client connection the daemon can watch for a complete close.
pub(super) trait ClientTransport {
    /// The watch for this connection, or `None` when the transport reports a
    /// peer's close only through reads and writes.
    fn hangup_watch(&self) -> Option<HangupWatch>;
}

impl ClientTransport for UnixStream {
    fn hangup_watch(&self) -> Option<HangupWatch> {
        let duplicate = self
            .as_fd()
            .try_clone_to_owned()
            .inspect_err(|error| warn!(%error, "attach hangup watch not armed: descriptor duplication failed"))
            .ok()?;
        let watched = AsyncFd::with_interest(duplicate, Interest::WRITABLE)
            .inspect_err(
                |error| warn!(%error, "attach hangup watch not armed: reactor registration failed"),
            )
            .ok()?;
        Some(HangupWatch(Box::pin(async move {
            loop {
                let mut ready = match watched.ready(Interest::WRITABLE).await {
                    Ok(ready) => ready,
                    Err(error) => {
                        warn!(%error, "attach hangup watch stopped: readiness failed");
                        std::future::pending::<()>().await;
                        return;
                    }
                };
                if ready.ready().is_write_closed() {
                    return;
                }
                ready.clear_ready();
            }
        })))
    }
}

impl ClientTransport for TcpStream {
    fn hangup_watch(&self) -> Option<HangupWatch> {
        None
    }
}

impl ClientTransport for DuplexStream {
    fn hangup_watch(&self) -> Option<HangupWatch> {
        None
    }
}
