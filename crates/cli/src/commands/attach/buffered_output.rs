//! Single owner of the terminal output of an attach.
//!
//! Every writer of the attach loop (stream output, dialog paints, shortcut
//! flushes, terminal restore) appends to one byte queue, and only a dedicated
//! loop arm, `flush` or `shutdown` moves the queue to the terminal. A slow
//! terminal therefore never keeps the loop from reading terminal input, and a
//! partially written chunk is only ever the head of the queue, so dialog escape
//! sequences cannot land inside it.

use std::io;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use tokio::io::AsyncWrite;

/// Bytes the queue accepts before a writer waits for the terminal.
pub(super) const OUTPUT_QUEUE_BYTES: usize = 32 * 1024;

#[derive(Debug)]
pub(super) struct BufferedOutput<O> {
    inner: O,
    queue: Vec<u8>,
    /// Queue bytes the terminal already took.
    written: usize,
}

impl<O: AsyncWrite + Unpin> BufferedOutput<O> {
    pub(super) fn new(inner: O) -> Self {
        Self {
            inner,
            queue: Vec::new(),
            written: 0,
        }
    }

    /// Bytes queued for the terminal.
    pub(super) fn queued(&self) -> usize {
        self.queue.len() - self.written
    }

    /// Writes the queue to the terminal and flushes it. Cancel-safe: the
    /// progress lives in the queue.
    pub(super) async fn drain(&mut self) -> io::Result<()> {
        std::future::poll_fn(|cx| self.poll_drain(cx)).await
    }

    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.written < self.queue.len() {
            let taken =
                ready!(Pin::new(&mut self.inner).poll_write(cx, &self.queue[self.written..]))?;
            if taken == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.written += taken;
        }
        self.queue.clear();
        self.written = 0;
        Pin::new(&mut self.inner).poll_flush(cx)
    }
}

impl<O: AsyncWrite + Unpin> AsyncWrite for BufferedOutput<O> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if self.queued() >= OUTPUT_QUEUE_BYTES {
            ready!(self.poll_drain(cx))?;
        }
        let room = OUTPUT_QUEUE_BYTES - self.queued();
        let accepted = room.min(buf.len());
        self.queue.extend_from_slice(&buf[..accepted]);
        Poll::Ready(Ok(accepted))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_drain(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.poll_drain(cx))?;
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
