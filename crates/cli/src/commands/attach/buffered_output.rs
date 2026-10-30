//! Single owner of the terminal output of an attach.
//!
//! Every writer of the attach loop (stream output, dialog paints, shortcut
//! flushes, terminal restore) appends to one byte queue, and only a dedicated
//! loop arm, `flush` or `shutdown` moves the queue to the terminal. A write
//! never waits for the terminal, so a slow terminal does not keep the loop from
//! reading terminal input, and a partially written chunk is only ever the head
//! of the queue, so dialog escape sequences cannot land inside it. The caller
//! bounds the queue by not reading more stream output while it holds
//! [`OUTPUT_QUEUE_BYTES`].

use std::io;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use tokio::io::AsyncWrite;

/// Queued bytes at which the caller stops reading stream output.
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
            if self.written >= OUTPUT_QUEUE_BYTES {
                self.queue.drain(..self.written);
                self.written = 0;
            }
        }
        self.queue.clear();
        self.written = 0;
        Pin::new(&mut self.inner).poll_flush(cx)
    }
}

impl<O: AsyncWrite + Unpin> AsyncWrite for BufferedOutput<O> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        self.queue.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_drain(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.poll_drain(cx))?;
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
