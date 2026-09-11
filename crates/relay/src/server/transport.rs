//! Bounds accepted sockets, TLS handshakes, and total connection lifetime.

use std::{
    future::Future,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::{OwnedSemaphorePermit, Semaphore},
    time::Sleep,
};

/// Carries explicit listener settings through axum-server's accept boundary.
#[derive(Clone, Debug)]
pub(crate) struct BoundedAddress {
    pub(crate) socket: std::net::SocketAddr,
    pub(crate) limits: crate::config::Limits,
    pub(crate) shutdown: tokio_util::sync::CancellationToken,
}

impl axum_server::Address for BoundedAddress {
    type Stream = BoundedStream<tokio::net::TcpStream>;
    type Listener = BoundedListener;
}

/// Rejects excess sockets before axum-server can allocate per-connection tasks.
#[derive(Debug)]
pub(crate) struct BoundedListener {
    socket: tokio::net::TcpListener,
    capacity: Arc<Semaphore>,
    limits: crate::config::Limits,
    shutdown: tokio_util::sync::CancellationToken,
}

impl axum_server::AddrListener<BoundedStream<tokio::net::TcpStream>, BoundedAddress>
    for BoundedListener
{
    // The trait mandates a future-returning signature. Binding is fully
    // synchronous (socket create + bind + listen), so the future is a plain
    // `ready` wrapper: `async fn` would trip `clippy::unused_async_trait_impl`,
    // a lint that postdates the workspace MSRV.
    fn bind_to(addr: BoundedAddress) -> impl Future<Output = io::Result<Self>> {
        std::future::ready(Self::bind_sync(addr))
    }

    async fn accept_stream(
        &self,
    ) -> io::Result<(BoundedStream<tokio::net::TcpStream>, BoundedAddress)> {
        loop {
            // At most one extra socket is briefly held by this single accept
            // loop. Rejected sockets never reach a TLS or HTTP task.
            let (socket, peer) = self.socket.accept().await?;
            let Ok(permit) = Arc::clone(&self.capacity).try_acquire_owned() else {
                drop(socket);
                continue;
            };
            socket.set_nodelay(true)?;
            let stream = BoundedStream {
                inner: socket,
                _permit: permit,
                deadline: Box::pin(tokio::time::sleep(self.limits.connection_lifetime)),
                cancelled: Box::pin(self.shutdown.clone().cancelled_owned()),
            };
            return Ok((
                stream,
                BoundedAddress {
                    socket: peer,
                    limits: self.limits.clone(),
                    shutdown: self.shutdown.clone(),
                },
            ));
        }
    }

    fn get_local_addr(&self) -> io::Result<BoundedAddress> {
        Ok(BoundedAddress {
            socket: self.socket.local_addr()?,
            limits: self.limits.clone(),
            shutdown: self.shutdown.clone(),
        })
    }
}

impl BoundedListener {
    fn bind_sync(addr: BoundedAddress) -> io::Result<Self> {
        let socket = if addr.socket.is_ipv4() {
            tokio::net::TcpSocket::new_v4()?
        } else {
            tokio::net::TcpSocket::new_v6()?
        };
        socket.bind(addr.socket)?;
        Ok(Self {
            socket: socket.listen(addr.limits.backlog)?,
            capacity: Arc::new(Semaphore::new(addr.limits.connections)),
            limits: addr.limits,
            shutdown: addr.shutdown,
        })
    }
}

/// The permit covers active, idle, and slow response sockets alike.
#[derive(Debug)]
pub(crate) struct BoundedStream<I> {
    inner: I,
    _permit: OwnedSemaphorePermit,
    deadline: Pin<Box<Sleep>>,
    cancelled: Pin<Box<tokio_util::sync::WaitForCancellationFutureOwned>>,
}

fn expired() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "connection deadline reached")
}

impl<I: AsyncRead + Unpin> AsyncRead for BoundedStream<I> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "relay stopped",
            )));
        }
        if self.deadline.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(expired()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<I: AsyncWrite + Unpin> AsyncWrite for BoundedStream<I> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "relay stopped",
            )));
        }
        if self.deadline.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(expired()));
        }
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "relay stopped",
            )));
        }
        if self.deadline.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(expired()));
        }
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "relay stopped",
            )));
        }
        if self.deadline.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(expired()));
        }
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum_server::AddrListener;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn listener_rejects_flood_before_returning_another_stream_and_reuses_capacity() {
        let limits = crate::config::fixture_limits();
        let listener = Arc::new(
            BoundedListener::bind_to(BoundedAddress {
                socket: "127.0.0.1:0".parse().expect("loopback"),
                limits,
                shutdown: tokio_util::sync::CancellationToken::new(),
            })
            .await
            .expect("listener"),
        );
        let address = listener.get_local_addr().expect("address").socket;
        let client = tokio::net::TcpStream::connect(address)
            .await
            .expect("first client");
        let (held, _) = listener
            .accept_stream()
            .await
            .expect("first accepted socket");
        let accepting = Arc::clone(&listener);
        let task = tokio::spawn(async move { accepting.accept_stream().await });
        for _ in 0..32 {
            let mut excess = tokio::net::TcpStream::connect(address)
                .await
                .expect("excess client");
            let mut byte = [0];
            let count = tokio::time::timeout(Duration::from_secs(1), excess.read(&mut byte))
                .await
                .expect("rejection bounded")
                .expect("closed excess socket");
            assert_eq!(count, 0);
            assert!(
                !task.is_finished(),
                "no excess stream reaches a server task"
            );
            assert_eq!(listener.capacity.available_permits(), 0);
        }
        drop(held);
        drop(client);
        let _client = tokio::net::TcpStream::connect(address)
            .await
            .expect("replacement client");
        let (stream, _) = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("reusable capacity")
            .expect("accept task")
            .expect("accepted replacement");
        drop(stream);
        assert_eq!(listener.capacity.available_permits(), 1);
    }

    #[tokio::test]
    async fn idle_socket_expires_without_another_request() {
        let (inner, _peer) = tokio::io::duplex(16);
        let capacity = Arc::new(Semaphore::new(1));
        let mut stream = BoundedStream {
            inner,
            _permit: capacity.acquire_owned().await.expect("capacity"),
            deadline: Box::pin(tokio::time::sleep(Duration::from_millis(10))),
            cancelled: Box::pin(tokio_util::sync::CancellationToken::new().cancelled_owned()),
        };
        let error = stream.read_u8().await.expect_err("idle lifetime expires");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
