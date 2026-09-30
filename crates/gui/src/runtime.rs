//! Tokio bridge used by the Iced shell.

// Rust guideline compliant 2026-06-26
#![forbid(unsafe_code)]

use std::future::Future;
use std::sync::LazyLock;

use futures::channel::{mpsc, oneshot};
use futures::{SinkExt, Stream, StreamExt};
use pohunek_gui_core::{
    workspace_connection_stream, ConnectionOptions, DomainEvent as CoreEvent, HostConfig,
};
use tokio::runtime::{Builder, Runtime};

static TOKIO: LazyLock<Runtime> = LazyLock::new(|| {
    Builder::new_multi_thread()
        .enable_all()
        .thread_name("pohunek-gui-tokio")
        .build()
        .expect("build pohunek-gui tokio runtime")
});

pub(crate) fn perform<F, T>(future: F) -> impl Future<Output = T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (sender, receiver) = oneshot::channel();
    TOKIO.spawn(async move {
        let output = future.await;
        let _ = sender.send(output);
    });
    async move { receiver.await.expect("tokio task completed") }
}

/// Runs blocking `work` on the runtime's blocking pool.
///
/// A panic inside `work` surfaces as an `Err` instead of tearing down the
/// caller.
pub(crate) fn perform_blocking<F>(work: F) -> impl Future<Output = Result<(), String>>
where
    F: FnOnce() -> Result<(), String> + Send + 'static,
{
    perform_blocking_or(work, Err)
}

/// Like [`perform_blocking`] for any output; `on_panic` maps a failed task
/// to a value of the output type.
pub(crate) fn perform_blocking_or<F, T>(
    work: F,
    on_panic: impl FnOnce(String) -> T,
) -> impl Future<Output = T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let handle = TOKIO.spawn_blocking(work);
    async move {
        match handle.await {
            Ok(output) => output,
            Err(err) => on_panic(format!("background task failed: {err}")),
        }
    }
}

pub(crate) fn host_subscription(
    input: &(HostConfig, ConnectionOptions),
) -> impl Stream<Item = CoreEvent> {
    let (mut sender, receiver) = mpsc::channel(128);
    let (config, options) = input.clone();
    TOKIO.spawn(async move {
        let mut stream = Box::pin(workspace_connection_stream(vec![config], options));
        while let Some(message) = stream.next().await {
            if sender.send(message).await.is_err() {
                break;
            }
        }
    });
    receiver
}
